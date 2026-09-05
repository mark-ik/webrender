// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Phase 6 image execution plan.
//!
//! Builders register graph-local logical images and closed image tasks. The
//! graph validates accesses, culls declarations unreachable from requested
//! outputs, and compiles a deterministic task order. Execution then binds
//! imported physical textures, allocates transient outputs, encodes each
//! closed render pass into one encoder batch, and returns requested textures.

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

/// Commands recorded inside one executor-owned render pass.
///
/// The executor, rather than the caller, opens and closes the pass against the
/// task's declared output. This is the cross-crate seam that keeps raw command
/// encoders out of the render-task API.
pub trait ImageRenderCommands: Send {
    fn encode<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>);
}

struct ImageRenderCommandClosure<F>(F);

impl<F> ImageRenderCommands for ImageRenderCommandClosure<F>
where
    F: for<'pass> Fn(&mut wgpu::RenderPass<'pass>) + Send,
{
    fn encode<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) {
        (self.0)(pass);
    }
}

/// Package render-pass commands without exposing the executor's encoder.
pub fn image_render_commands<F>(commands: F) -> Box<dyn ImageRenderCommands>
where
    F: for<'pass> Fn(&mut wgpu::RenderPass<'pass>) + Send + 'static,
{
    Box::new(ImageRenderCommandClosure(commands))
}

/// Prepare one task after its declared input views have been resolved.
/// Resource creation belongs here; pass commands belong in the returned
/// [`ImageRenderCommands`].
pub type PrepareCallback =
    Box<dyn FnOnce(&wgpu::Device, &[wgpu::TextureView]) -> Box<dyn ImageRenderCommands> + Send>;

/// Process-local identity for one logical graph instance.
///
/// This is deliberately separate from retained-scene generations. It is used
/// only to refuse a node copied from another graph during admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphId(u64);

/// Graph-local logical image resource. This is never a physical wgpu texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageNode {
    graph: GraphId,
    index: u32,
}

impl ImageNode {
    /// Stable logical numbering used by the optional plan dump.
    #[allow(dead_code)]
    fn logical_index(self) -> u32 {
        self.index
    }
}

/// Descriptor for an image allocated by the graph executor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TransientImageDesc {
    pub size: wgpu::Extent3d,
    pub format: wgpu::TextureFormat,
    pub usage: wgpu::TextureUsages,
    pub label: Option<String>,
}

/// The first image-only access vocabulary. wgpu remains responsible for the
/// physical transitions implied by these declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageAccess {
    SampledRead,
    ColorAttachment { load: ImageLoad, store: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageLoad {
    Clear,
    /// Reserved for a future load-preserving attachment task.
    #[allow(dead_code)]
    Load,
}

/// A named image access on a planned task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageUse {
    pub image: ImageNode,
    pub access: ImageAccess,
}

impl ImageUse {
    pub fn sampled_read(image: ImageNode) -> Self {
        Self {
            image,
            access: ImageAccess::SampledRead,
        }
    }

    pub fn color_attachment(image: ImageNode, load: ImageLoad) -> Self {
        Self {
            image,
            access: ImageAccess::ColorAttachment { load, store: true },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ImageDecl {
    Imported {
        label: String,
        size: wgpu::Extent3d,
        format: wgpu::TextureFormat,
    },
    Transient(TransientImageDesc),
}

/// A logical resource lifetime in the compiled task order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLifetime {
    pub image: ImageNode,
    pub first_use: usize,
    pub last_use: usize,
}

/// Per-descriptor logical allocation and liveness accounting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorReport {
    pub size: wgpu::Extent3d,
    pub format: wgpu::TextureFormat,
    pub usage: wgpu::TextureUsages,
    pub transient_creations: usize,
    pub estimated_bytes: Option<u64>,
    pub peak_live_count: usize,
    pub peak_live_bytes: Option<u64>,
}

/// CPU-side execution receipt. Durations describe host work only; they are not
/// GPU timestamps or physical allocator measurements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    pub compile_duration: Duration,
    pub allocate_duration: Duration,
    pub encode_duration: Duration,
    pub submit_duration: Duration,
    pub transient_creations: usize,
    pub logical_created_bytes: Option<u64>,
    pub peak_live_count: usize,
    pub peak_live_bytes: Option<u64>,
    pub descriptors: Vec<DescriptorReport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphBuildError {
    ForeignImageNode {
        expected: GraphId,
        found: GraphId,
    },
    UnknownImageNode {
        image: ImageNode,
    },
    DuplicateProducer {
        image: ImageNode,
    },
    MissingProducer {
        image: ImageNode,
    },
    NoRequestedOutputs,
    Cycle {
        task_labels: Vec<String>,
    },
    InvalidInputAccess {
        task_label: String,
        access: ImageAccess,
    },
    InvalidOutputAccess {
        task_label: String,
        access: ImageAccess,
    },
    #[allow(dead_code)]
    OutputMustBeTransient {
        image: ImageNode,
    },
    MissingImageUsage {
        task_label: String,
        image: ImageNode,
        required: wgpu::TextureUsages,
    },
    InvalidOutputLoad {
        task_label: String,
    },
    InvalidImportedOutputAccess {
        task_label: String,
        access: ImageAccess,
    },
}

impl fmt::Display for GraphBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignImageNode { expected, found } => {
                write!(
                    f,
                    "image node belongs to graph {found:?}, expected {expected:?}"
                )
            }
            Self::UnknownImageNode { image } => write!(f, "unknown image node {image:?}"),
            Self::DuplicateProducer { image } => {
                write!(f, "image node {image:?} has more than one producer")
            }
            Self::MissingProducer { image } => {
                write!(f, "transient image {image:?} has no producer")
            }
            Self::NoRequestedOutputs => write!(f, "execution graph has no requested outputs"),
            Self::Cycle { task_labels } => {
                write!(
                    f,
                    "execution graph cycle leaves tasks {task_labels:?} blocked"
                )
            }
            Self::InvalidInputAccess { task_label, access } => {
                write!(f, "task {task_label:?} has invalid input access {access:?}")
            }
            Self::InvalidOutputAccess { task_label, access } => {
                write!(
                    f,
                    "task {task_label:?} has invalid output access {access:?}"
                )
            }
            Self::OutputMustBeTransient { image } => {
                write!(f, "task output {image:?} must be a transient image")
            }
            Self::MissingImageUsage {
                task_label,
                image,
                required,
            } => write!(
                f,
                "task {task_label:?} image {image:?} lacks required usage {required:?}"
            ),
            Self::InvalidOutputLoad { task_label } => {
                write!(
                    f,
                    "task {task_label:?} cannot load a fresh transient output"
                )
            }
            Self::InvalidImportedOutputAccess { task_label, access } => write!(
                f,
                "task {task_label:?} imported output requires ColorAttachment {{ load: Load, store: true }}, got {access:?}"
            ),
        }
    }
}

impl Error for GraphBuildError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphExecutionError {
    MissingImportedImage {
        image: ImageNode,
    },
    UnknownImportedImage {
        image: ImageNode,
    },
    ImportedImageForTransient {
        image: ImageNode,
    },
    OutputUnavailable {
        image: ImageNode,
    },
    ImportedImageMismatch {
        image: ImageNode,
        expected_size: wgpu::Extent3d,
        actual_size: wgpu::Extent3d,
        expected_format: wgpu::TextureFormat,
        actual_format: wgpu::TextureFormat,
        required_usage: wgpu::TextureUsages,
        actual_usage: wgpu::TextureUsages,
    },
}

impl fmt::Display for GraphExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingImportedImage { image } => {
                write!(f, "missing imported image binding for {image:?}")
            }
            Self::UnknownImportedImage { image } => {
                write!(f, "binding supplied for unknown image {image:?}")
            }
            Self::ImportedImageForTransient { image } => {
                write!(f, "binding supplied for transient image {image:?}")
            }
            Self::OutputUnavailable { image } => {
                write!(
                    f,
                    "compiled output {image:?} was unavailable after execution"
                )
            }
            Self::ImportedImageMismatch {
                image,
                expected_size,
                actual_size,
                expected_format,
                actual_format,
                required_usage,
                actual_usage,
            } => write!(
                f,
                "imported image {image:?} mismatch: size {actual_size:?} (expected {expected_size:?}), format {actual_format:?} (expected {expected_format:?}), usage {actual_usage:?} (requires {required_usage:?})"
            ),
        }
    }
}

impl Error for GraphExecutionError {}

fn find_decl<'a>(images: &'a [(ImageNode, ImageDecl)], image: ImageNode) -> Option<&'a ImageDecl> {
    images
        .iter()
        .find_map(|(node, decl)| (*node == image).then_some(decl))
}

fn imported_image_matches(
    expected_size: wgpu::Extent3d,
    expected_format: wgpu::TextureFormat,
    required_usage: wgpu::TextureUsages,
    actual_size: wgpu::Extent3d,
    actual_format: wgpu::TextureFormat,
    actual_usage: wgpu::TextureUsages,
) -> bool {
    actual_size == expected_size
        && actual_format == expected_format
        && actual_usage.contains(required_usage)
}

fn image_bytes(size: wgpu::Extent3d, format: wgpu::TextureFormat) -> Option<u64> {
    let (block_width, block_height) = format.block_dimensions();
    let blocks_x = u64::from(size.width.div_ceil(block_width));
    let blocks_y = u64::from(size.height.div_ceil(block_height));
    u64::from(format.block_copy_size(None)? as u32)
        .checked_mul(blocks_x)
        .and_then(|bytes| bytes.checked_mul(blocks_y))
        .and_then(|bytes| bytes.checked_mul(u64::from(size.depth_or_array_layers)))
}

fn build_report(
    images: &[(ImageNode, ImageDecl)],
    _tasks: &[CompiledTask],
    _requested_outputs: &[ImageNode],
    lifetimes: &[ResourceLifetime],
    compile_duration: Duration,
) -> ExecutionReport {
    let mut descriptors = Vec::<DescriptorReport>::new();
    for lifetime in lifetimes {
        let Some(ImageDecl::Transient(desc)) = find_decl(images, lifetime.image) else {
            continue;
        };
        let entry = if let Some(index) = descriptors.iter().position(|entry| {
            entry.size == desc.size && entry.format == desc.format && entry.usage == desc.usage
        }) {
            &mut descriptors[index]
        } else {
            descriptors.push(DescriptorReport {
                size: desc.size,
                format: desc.format,
                usage: desc.usage,
                transient_creations: 0,
                estimated_bytes: image_bytes(desc.size, desc.format),
                peak_live_count: 0,
                peak_live_bytes: image_bytes(desc.size, desc.format),
            });
            descriptors.last_mut().expect("descriptor just inserted")
        };
        entry.transient_creations += 1;
    }

    let task_count = lifetimes
        .iter()
        .map(|lifetime| lifetime.last_use + 1)
        .max()
        .unwrap_or(0);
    let mut peak_live_count = 0usize;
    let mut peak_live_bytes = Some(0u64);
    for step in 0..task_count {
        let live = lifetimes
            .iter()
            .filter(|l| l.first_use <= step && step <= l.last_use)
            .collect::<Vec<_>>();
        peak_live_count = peak_live_count.max(live.len());
        let mut live_bytes = Some(0u64);
        for lifetime in live {
            let Some(ImageDecl::Transient(desc)) = find_decl(images, lifetime.image) else {
                continue;
            };
            live_bytes =
                live_bytes.and_then(|sum| image_bytes(desc.size, desc.format)?.checked_add(sum));
        }
        peak_live_bytes = match (peak_live_bytes, live_bytes) {
            (Some(previous), Some(current)) => Some(previous.max(current)),
            _ => None,
        };
    }

    // Recompute per-descriptor peak counts in one deterministic pass.
    for entry in &mut descriptors {
        entry.peak_live_count = 0;
        entry.peak_live_bytes = entry.estimated_bytes.map(|bytes| bytes);
        for step in 0..task_count {
            let live_count = lifetimes
                .iter()
                .filter(|l| l.first_use <= step && step <= l.last_use)
                .filter(|l| match find_decl(images, l.image) {
                    Some(ImageDecl::Transient(desc)) => {
                        desc.size == entry.size
                            && desc.format == entry.format
                            && desc.usage == entry.usage
                    }
                    Some(ImageDecl::Imported { .. }) | None => false,
                })
                .count();
            entry.peak_live_count = entry.peak_live_count.max(live_count);
            if let Some(bytes) = entry.estimated_bytes {
                entry.peak_live_bytes = Some(
                    entry
                        .peak_live_bytes
                        .unwrap_or(0)
                        .max(bytes.saturating_mul(live_count as u64)),
                );
            } else {
                entry.peak_live_bytes = None;
            }
        }
    }

    let logical_created_bytes = descriptors.iter().try_fold(0u64, |sum, entry| {
        entry
            .estimated_bytes?
            .checked_mul(entry.transient_creations as u64)?
            .checked_add(sum)
    });
    ExecutionReport {
        compile_duration,
        allocate_duration: Duration::ZERO,
        encode_duration: Duration::ZERO,
        submit_duration: Duration::ZERO,
        transient_creations: descriptors
            .iter()
            .map(|entry| entry.transient_creations)
            .sum(),
        logical_created_bytes,
        peak_live_count,
        peak_live_bytes,
        descriptors,
    }
}

struct PlannedTask {
    label: String,
    inputs: Vec<ImageUse>,
    output: ImageUse,
    prepare: PrepareCallback,
}

struct CompiledTask {
    /// Retained for deterministic plan diagnostics.
    #[allow(dead_code)]
    label: String,
    inputs: Vec<ImageUse>,
    output: ImageUse,
    prepare: PrepareCallback,
}

/// A validated, cullable image execution plan. It owns opaque encode
/// callbacks but performs no device allocation until [`Self::execute`].
pub struct ExecutionPlan {
    graph: GraphId,
    images: Vec<(ImageNode, ImageDecl)>,
    tasks: Vec<CompiledTask>,
    requested_outputs: Vec<ImageNode>,
    lifetimes: Vec<ResourceLifetime>,
    compile_duration: Duration,
    diagnostic_header: Option<String>,
}

impl ExecutionPlan {
    /// Stable, handle-free diagnostic representation of this plan.
    #[allow(dead_code)]
    pub fn dump(&self) -> String {
        let mut out = String::from("ExecutionPlan\n");
        if let Some(header) = &self.diagnostic_header {
            out.push_str(header);
            out.push('\n');
        } else {
            out.push_str("rasterizer=unspecified execution_boundary=unspecified\n");
        }
        out.push_str("resources:\n");
        for (node, decl) in &self.images {
            let index = node.logical_index();
            match decl {
                ImageDecl::Imported {
                    label,
                    size,
                    format,
                } => {
                    out.push_str(&format!(
                        "  image#{index} imported {label:?} {}x{}x{} {format:?}\n",
                        size.width, size.height, size.depth_or_array_layers
                    ));
                }
                ImageDecl::Transient(desc) => {
                    out.push_str(&format!(
                        "  image#{index} transient {}x{}x{} {:?} usage={:?} label={:?}\n",
                        desc.size.width,
                        desc.size.height,
                        desc.size.depth_or_array_layers,
                        desc.format,
                        desc.usage,
                        desc.label
                    ));
                }
            }
        }
        out.push_str("selected_outputs: [");
        for (index, image) in self.requested_outputs.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("image#{}", image.logical_index()));
        }
        out.push_str("]\nsteps:\n");
        for (index, task) in self.tasks.iter().enumerate() {
            out.push_str(&format!("  {index}: {:?} inputs=[", task.label));
            for (input_index, input) in task.inputs.iter().enumerate() {
                if input_index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!(
                    "image#{} {:?}",
                    input.image.logical_index(),
                    input.access
                ));
            }
            out.push_str(&format!(
                "] output=image#{} {:?} edges=[",
                task.output.image.logical_index(),
                task.output.access
            ));
            for (edge_index, input) in task.inputs.iter().enumerate() {
                if edge_index > 0 {
                    out.push_str(", ");
                }
                let producer = self
                    .tasks
                    .iter()
                    .position(|candidate| candidate.output.image == input.image)
                    .map(|producer| format!("{producer}->{}", index))
                    .unwrap_or_else(|| format!("import->{}", index));
                out.push_str(&producer);
            }
            out.push_str("]\n");
        }
        out.push_str("lifetimes:\n");
        for lifetime in &self.lifetimes {
            out.push_str(&format!(
                "  image#{} {}..={}\n",
                lifetime.image.logical_index(),
                lifetime.first_use,
                lifetime.last_use
            ));
        }
        out.push_str("graph_encoder_batches: 1\ngraph_submission_boundaries: 1 (submit)\n");
        out
    }

    /// Attach a producer-owned diagnostic header to this plan.
    /// The text is diagnostic only; it does not alter graph scheduling.
    #[allow(dead_code)]
    pub fn with_diagnostic_header(mut self, header: impl Into<String>) -> Self {
        self.diagnostic_header = Some(header.into());
        self
    }

    /// Resource lifetimes retained for allocator/report consumers.
    #[allow(dead_code)]
    pub fn lifetimes(&self) -> &[ResourceLifetime] {
        &self.lifetimes
    }

    /// Return deterministic logical allocation metrics without touching a GPU.
    #[allow(dead_code)]
    pub fn logical_report(&self) -> ExecutionReport {
        build_report(
            self.images.as_slice(),
            self.tasks.as_slice(),
            self.requested_outputs.as_slice(),
            self.lifetimes.as_slice(),
            self.compile_duration,
        )
    }

    /// Encode one image-only batch into a caller-owned encoder. Imported
    /// bindings own the texture values passed here; transient outputs are
    /// owned by the returned map. This is the seam for a rasterizer prelude
    /// that must share the graph executor's submission.
    pub fn encode_into(
        self,
        device: &wgpu::Device,
        mut imported: HashMap<ImageNode, wgpu::Texture>,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<(HashMap<ImageNode, wgpu::Texture>, ExecutionReport), GraphExecutionError> {
        for image in imported.keys().copied().collect::<Vec<_>>() {
            let Some(decl) = find_decl(&self.images, image) else {
                return Err(GraphExecutionError::UnknownImportedImage { image });
            };
            if !matches!(decl, ImageDecl::Imported { .. }) {
                return Err(GraphExecutionError::ImportedImageForTransient { image });
            }
            if image.graph != self.graph {
                return Err(GraphExecutionError::UnknownImportedImage { image });
            }
        }
        for (image, decl) in &self.images {
            if matches!(decl, ImageDecl::Imported { .. }) {
                let Some(texture) = imported.get(image) else {
                    return Err(GraphExecutionError::MissingImportedImage { image: *image });
                };
                let ImageDecl::Imported {
                    size: expected_size,
                    format: expected_format,
                    ..
                } = decl
                else {
                    continue;
                };
                let required_usage =
                    self.tasks
                        .iter()
                        .fold(wgpu::TextureUsages::empty(), |usage, task| {
                            let usage = task
                                .inputs
                                .iter()
                                .filter(|input| input.image == *image)
                                .fold(usage, |usage, input| {
                                    if matches!(input.access, ImageAccess::SampledRead) {
                                        usage | wgpu::TextureUsages::TEXTURE_BINDING
                                    } else {
                                        usage
                                    }
                                });
                            if task.output.image == *image
                                && matches!(task.output.access, ImageAccess::ColorAttachment { .. })
                            {
                                usage | wgpu::TextureUsages::RENDER_ATTACHMENT
                            } else {
                                usage
                            }
                        });
                let actual_size = texture.size();
                let actual_format = texture.format();
                let actual_usage = texture.usage();
                if !imported_image_matches(
                    *expected_size,
                    *expected_format,
                    required_usage,
                    actual_size,
                    actual_format,
                    actual_usage,
                ) {
                    return Err(GraphExecutionError::ImportedImageMismatch {
                        image: *image,
                        expected_size: *expected_size,
                        actual_size,
                        expected_format: *expected_format,
                        actual_format,
                        required_usage,
                        actual_usage,
                    });
                }
            }
        }

        let mut outputs = std::mem::take(&mut imported);
        let allocate_start = Instant::now();
        let mut allocated = Vec::with_capacity(self.tasks.len());
        for task in &self.tasks {
            let texture = match find_decl(&self.images, task.output.image) {
                Some(ImageDecl::Transient(desc)) => {
                    device.create_texture(&wgpu::TextureDescriptor {
                        label: desc.label.as_deref(),
                        size: desc.size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: desc.format,
                        usage: desc.usage,
                        view_formats: &[],
                    })
                }
                Some(ImageDecl::Imported { .. }) => outputs
                    .get(&task.output.image)
                    .cloned()
                    .ok_or(GraphExecutionError::OutputUnavailable {
                        image: task.output.image,
                    })?,
                None => {
                    return Err(GraphExecutionError::OutputUnavailable {
                        image: task.output.image,
                    });
                }
            };
            allocated.push((task.output.image, texture));
        }
        let allocate_duration = allocate_start.elapsed();

        let encode_start = Instant::now();
        let requested_outputs = self.requested_outputs.clone();
        let lifetimes = self.lifetimes.clone();
        for (task_index, (task, (image, output_texture))) in self
            .tasks
            .into_iter()
            .zip(allocated.into_iter())
            .enumerate()
        {
            let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let input_views: Vec<wgpu::TextureView> = task
                .inputs
                .iter()
                .map(|input| {
                    outputs
                        .get(&input.image)
                        .expect("validated input available before planned callback")
                        .create_view(&wgpu::TextureViewDescriptor::default())
                })
                .collect();

            let commands = (task.prepare)(device, &input_views);
            let ImageAccess::ColorAttachment { load, store } = task.output.access else {
                unreachable!("validated task output is a color attachment")
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(&task.label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: match load {
                            ImageLoad::Clear => wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            ImageLoad::Load => wgpu::LoadOp::Load,
                        },
                        store: if store {
                            wgpu::StoreOp::Store
                        } else {
                            wgpu::StoreOp::Discard
                        },
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            commands.encode(&mut pass);
            drop(pass);
            if matches!(
                find_decl(&self.images, image),
                Some(ImageDecl::Transient(_))
            ) {
                outputs.insert(image, output_texture);
            }

            // Keep selected outputs and resources needed by later tasks. This
            // models the compiled lifetime rather than retaining every output.
            let expired = outputs
                .keys()
                .copied()
                .filter(|candidate| {
                    !requested_outputs.contains(candidate)
                        && self.lifetimes.iter().any(|lifetime| {
                            lifetime.image == *candidate && lifetime.last_use == task_index
                        })
                        && matches!(
                            find_decl(&self.images, *candidate),
                            Some(ImageDecl::Transient(_))
                        )
                })
                .collect::<Vec<_>>();
            for candidate in expired {
                outputs.remove(&candidate);
            }
        }
        let encode_duration = encode_start.elapsed();

        let mut report = build_report(
            &self.images,
            &[],
            &requested_outputs,
            &lifetimes,
            self.compile_duration,
        );
        report.allocate_duration = allocate_duration;
        report.encode_duration = encode_duration;
        Ok((outputs, report))
    }

    /// Execute one image-only encoder batch. Imported bindings own the texture
    /// values passed here; transient outputs are owned by the returned map.
    pub fn execute(
        self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        imported: HashMap<ImageNode, wgpu::Texture>,
    ) -> Result<(HashMap<ImageNode, wgpu::Texture>, ExecutionReport), GraphExecutionError> {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("netrender execution plan"),
        });
        let (outputs, mut report) = self.encode_into(device, imported, &mut encoder)?;
        let submit_start = Instant::now();
        queue.submit(std::iter::once(encoder.finish()));
        report.submit_duration = submit_start.elapsed();
        Ok((outputs, report))
    }
}

/// Directed acyclic graph of image tasks.
pub struct RenderGraph {
    graph: GraphId,
    images: Vec<ImageDecl>,
    planned_tasks: Vec<PlannedTask>,
}

impl RenderGraph {
    pub fn new() -> Self {
        static NEXT_GRAPH_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            graph: GraphId(NEXT_GRAPH_ID.fetch_add(1, Ordering::Relaxed)),
            images: Vec::new(),
            planned_tasks: Vec::new(),
        }
    }

    /// Register a caller-owned image. The physical texture is supplied to
    /// [`ExecutionPlan::execute`] under the returned logical node.
    pub fn import_image(
        &mut self,
        label: impl Into<String>,
        size: wgpu::Extent3d,
        format: wgpu::TextureFormat,
    ) -> ImageNode {
        let node = ImageNode {
            graph: self.graph,
            index: self.images.len() as u32,
        };
        self.images.push(ImageDecl::Imported {
            label: label.into(),
            size,
            format,
        });
        node
    }

    /// Register an executor-owned transient image.
    pub fn transient_image(&mut self, desc: TransientImageDesc) -> ImageNode {
        let node = ImageNode {
            graph: self.graph,
            index: self.images.len() as u32,
        };
        self.images.push(ImageDecl::Transient(desc));
        node
    }

    /// Add one image-only task to the build/compile path. The callback prepares
    /// resources, then returns commands for the executor-owned output pass.
    /// It cannot access the raw command encoder or choose another target.
    pub fn add_plan_task(
        &mut self,
        label: impl Into<String>,
        inputs: Vec<ImageUse>,
        output: ImageUse,
        prepare: PrepareCallback,
    ) -> Result<(), GraphBuildError> {
        let task = PlannedTask {
            label: label.into(),
            inputs,
            output,
            prepare,
        };
        for use_ in task.inputs.iter().chain(std::iter::once(&task.output)) {
            self.check_node(use_.image)?;
        }
        if let Some(input) = task
            .inputs
            .iter()
            .find(|input| !matches!(input.access, ImageAccess::SampledRead))
        {
            return Err(GraphBuildError::InvalidInputAccess {
                task_label: task.label,
                access: input.access,
            });
        }
        if !matches!(task.output.access, ImageAccess::ColorAttachment { .. }) {
            return Err(GraphBuildError::InvalidOutputAccess {
                task_label: task.label,
                access: task.output.access,
            });
        }
        match &self.images[task.output.image.index as usize] {
            ImageDecl::Transient(_) => {}
            ImageDecl::Imported { .. } => {
                if !matches!(
                    task.output.access,
                    ImageAccess::ColorAttachment {
                        load: ImageLoad::Load,
                        store: true,
                    }
                ) {
                    return Err(GraphBuildError::InvalidImportedOutputAccess {
                        task_label: task.label.clone(),
                        access: task.output.access,
                    });
                }
            }
        }
        for input in &task.inputs {
            if let Some(ImageDecl::Transient(desc)) = self.images.get(input.image.index as usize) {
                if !desc.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
                    return Err(GraphBuildError::MissingImageUsage {
                        task_label: task.label.clone(),
                        image: input.image,
                        required: wgpu::TextureUsages::TEXTURE_BINDING,
                    });
                }
            }
        }
        if let ImageDecl::Transient(desc) = &self.images[task.output.image.index as usize] {
            if !desc.usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
                return Err(GraphBuildError::MissingImageUsage {
                    task_label: task.label.clone(),
                    image: task.output.image,
                    required: wgpu::TextureUsages::RENDER_ATTACHMENT,
                });
            }
        }
        if matches!(
            task.output.access,
            ImageAccess::ColorAttachment {
                load: ImageLoad::Load,
                ..
            }
        ) {
            if matches!(
                self.images[task.output.image.index as usize],
                ImageDecl::Transient(_)
            ) {
                return Err(GraphBuildError::InvalidOutputLoad {
                    task_label: task.label.clone(),
                });
            }
        }
        self.planned_tasks.push(task);
        Ok(())
    }

    fn check_node(&self, image: ImageNode) -> Result<(), GraphBuildError> {
        if image.graph != self.graph {
            return Err(GraphBuildError::ForeignImageNode {
                expected: self.graph,
                found: image.graph,
            });
        }
        if self.images.get(image.index as usize).is_none() {
            return Err(GraphBuildError::UnknownImageNode { image });
        }
        Ok(())
    }

    /// Consume the logical graph and produce an inspectable, cullable plan.
    /// Allocation and encoding happen only when the returned plan executes.
    pub fn compile(
        self,
        requested_outputs: &[ImageNode],
    ) -> Result<ExecutionPlan, GraphBuildError> {
        let compile_start = Instant::now();
        if requested_outputs.is_empty() {
            return Err(GraphBuildError::NoRequestedOutputs);
        }
        for &image in requested_outputs {
            self.check_node(image)?;
        }

        let mut producers = HashMap::with_capacity(self.planned_tasks.len());
        for (index, task) in self.planned_tasks.iter().enumerate() {
            for use_ in task.inputs.iter().chain(std::iter::once(&task.output)) {
                self.check_node(use_.image)?;
            }
            if let Some(input) = task
                .inputs
                .iter()
                .find(|input| !matches!(input.access, ImageAccess::SampledRead))
            {
                return Err(GraphBuildError::InvalidInputAccess {
                    task_label: task.label.clone(),
                    access: input.access,
                });
            }
            if !matches!(task.output.access, ImageAccess::ColorAttachment { .. }) {
                return Err(GraphBuildError::InvalidOutputAccess {
                    task_label: task.label.clone(),
                    access: task.output.access,
                });
            }
            if matches!(
                self.images[task.output.image.index as usize],
                ImageDecl::Imported { .. }
            ) && !matches!(
                task.output.access,
                ImageAccess::ColorAttachment {
                    load: ImageLoad::Load,
                    store: true,
                }
            ) {
                return Err(GraphBuildError::InvalidImportedOutputAccess {
                    task_label: task.label.clone(),
                    access: task.output.access,
                });
            }
            if matches!(
                self.images[task.output.image.index as usize],
                ImageDecl::Transient(_)
            ) && matches!(
                task.output.access,
                ImageAccess::ColorAttachment {
                    load: ImageLoad::Load,
                    ..
                }
            ) {
                return Err(GraphBuildError::InvalidOutputLoad {
                    task_label: task.label.clone(),
                });
            }
            if producers.insert(task.output.image, index).is_some() {
                return Err(GraphBuildError::DuplicateProducer {
                    image: task.output.image,
                });
            }
        }

        let mut needed = HashSet::new();
        let mut pending: Vec<ImageNode> = requested_outputs.to_vec();
        while let Some(image) = pending.pop() {
            if let Some(&producer) = producers.get(&image) {
                if needed.insert(producer) {
                    pending.extend(self.planned_tasks[producer].inputs.iter().map(|u| u.image));
                }
            } else if !matches!(
                self.images[image.index as usize],
                ImageDecl::Imported { .. }
            ) {
                return Err(GraphBuildError::MissingProducer { image });
            }
        }

        let mut indegree = vec![0usize; self.planned_tasks.len()];
        let mut dependents = vec![Vec::<usize>::new(); self.planned_tasks.len()];
        for index in 0..self.planned_tasks.len() {
            if !needed.contains(&index) {
                continue;
            }
            for input in &self.planned_tasks[index].inputs {
                if let Some(&producer) = producers.get(&input.image) {
                    if needed.contains(&producer) {
                        indegree[index] += 1;
                        dependents[producer].push(index);
                    }
                }
            }
        }
        let mut ready: VecDeque<usize> = (0..self.planned_tasks.len())
            .filter(|index| needed.contains(index) && indegree[*index] == 0)
            .collect();
        let mut order = Vec::with_capacity(needed.len());
        while let Some(index) = ready.pop_front() {
            order.push(index);
            for dependent in &dependents[index] {
                indegree[*dependent] -= 1;
                if indegree[*dependent] == 0 {
                    ready.push_back(*dependent);
                }
            }
        }
        if order.len() != needed.len() {
            let task_labels = self
                .planned_tasks
                .iter()
                .enumerate()
                .filter(|(index, _)| needed.contains(index) && indegree[*index] > 0)
                .map(|(_, task)| task.label.clone())
                .collect();
            return Err(GraphBuildError::Cycle { task_labels });
        }

        let mut planned_tasks: Vec<Option<PlannedTask>> =
            self.planned_tasks.into_iter().map(Some).collect();
        let tasks = order
            .into_iter()
            .map(|index| {
                let task = planned_tasks[index].take().expect("task index");
                CompiledTask {
                    label: task.label,
                    inputs: task.inputs,
                    output: task.output,
                    prepare: task.prepare,
                }
            })
            .collect::<Vec<_>>();
        let mut lifetimes: Vec<ResourceLifetime> = Vec::new();
        for (task_index, task) in tasks.iter().enumerate() {
            for use_ in task.inputs.iter().chain(std::iter::once(&task.output)) {
                if matches!(
                    self.images[use_.image.index as usize],
                    ImageDecl::Transient(_)
                ) {
                    if let Some(lifetime) = lifetimes.iter_mut().find(|l| l.image == use_.image) {
                        lifetime.first_use = lifetime.first_use.min(task_index);
                        lifetime.last_use = lifetime.last_use.max(task_index);
                    } else {
                        lifetimes.push(ResourceLifetime {
                            image: use_.image,
                            first_use: task_index,
                            last_use: task_index,
                        });
                    }
                }
            }
        }
        lifetimes.sort_by_key(|l| l.image.index);
        let selected: HashSet<ImageNode> = requested_outputs
            .iter()
            .copied()
            .chain(tasks.iter().flat_map(|task| {
                task.inputs
                    .iter()
                    .map(|input| input.image)
                    .chain(std::iter::once(task.output.image))
            }))
            .collect();
        for lifetime in &mut lifetimes {
            if requested_outputs.contains(&lifetime.image) {
                lifetime.last_use = tasks.len();
            }
        }
        let images = self
            .images
            .into_iter()
            .enumerate()
            .filter_map(|(index, decl)| {
                let image = ImageNode {
                    graph: self.graph,
                    index: index as u32,
                };
                selected.contains(&image).then_some((image, decl))
            })
            .collect();
        let compile_duration = compile_start.elapsed();
        Ok(ExecutionPlan {
            graph: self.graph,
            images,
            tasks,
            requested_outputs: requested_outputs.to_vec(),
            lifetimes,
            compile_duration,
            diagnostic_header: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_desc(size: wgpu::Extent3d, usage: wgpu::TextureUsages) -> TransientImageDesc {
        TransientImageDesc {
            size,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage,
            label: None,
        }
    }

    fn noop_callback() -> PrepareCallback {
        Box::new(|_, _| image_render_commands(|_| {}))
    }

    #[test]
    fn plan_rejects_duplicate_producers() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        let mut graph = RenderGraph::new();
        let output = graph.transient_image(plan_desc(size, usage));
        graph
            .add_plan_task(
                "first producer",
                Vec::new(),
                ImageUse::color_attachment(output, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "second producer",
                Vec::new(),
                ImageUse::color_attachment(output, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        assert!(matches!(
            graph.compile(&[output]),
            Err(GraphBuildError::DuplicateProducer { image }) if image == output
        ));
    }

    #[test]
    fn plan_rejects_cycles() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        let mut graph = RenderGraph::new();
        let left = graph.transient_image(plan_desc(size, usage));
        let right = graph.transient_image(plan_desc(size, usage));
        graph
            .add_plan_task(
                "left",
                vec![ImageUse::sampled_read(right)],
                ImageUse::color_attachment(left, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "right",
                vec![ImageUse::sampled_read(left)],
                ImageUse::color_attachment(right, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        assert!(matches!(
            graph.compile(&[left]),
            Err(GraphBuildError::Cycle { task_labels }) if task_labels == vec!["left", "right"]
        ));
    }

    #[test]
    fn plan_culls_disconnected_branch_and_reports_lifetimes() {
        let size = wgpu::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        };
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let mut graph = RenderGraph::new();
        let input = graph.import_image("input", size, wgpu::TextureFormat::Rgba8Unorm);
        let mask = graph.transient_image(plan_desc(size, usage));
        let horizontal = graph.transient_image(plan_desc(size, usage));
        let vertical = graph.transient_image(plan_desc(size, usage));
        let matrix = graph.transient_image(plan_desc(size, usage));
        let disconnected = graph.transient_image(plan_desc(size, usage));
        let disconnected_tail = graph.transient_image(plan_desc(size, usage));
        graph
            .add_plan_task(
                "mask",
                vec![ImageUse::sampled_read(input)],
                ImageUse::color_attachment(mask, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "horizontal blur",
                vec![ImageUse::sampled_read(mask)],
                ImageUse::color_attachment(horizontal, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "vertical blur",
                vec![ImageUse::sampled_read(horizontal)],
                ImageUse::color_attachment(vertical, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "color matrix",
                vec![ImageUse::sampled_read(vertical)],
                ImageUse::color_attachment(matrix, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "disconnected",
                vec![ImageUse::sampled_read(input)],
                ImageUse::color_attachment(disconnected, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "disconnected tail",
                vec![ImageUse::sampled_read(disconnected)],
                ImageUse::color_attachment(disconnected_tail, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();

        let plan = graph.compile(&[matrix]).unwrap();
        let dump = plan.dump();
        assert!(dump.contains("mask"));
        assert!(dump.contains("edges=[import->0]"));
        assert!(!dump.contains("disconnected"));
        let report = plan.logical_report();
        assert_eq!(report.transient_creations, 4);
        assert_eq!(report.logical_created_bytes, Some(64));
        assert_eq!(report.peak_live_count, 2);
        assert_eq!(report.peak_live_bytes, Some(32));
        assert_eq!(report.descriptors.len(), 1);
        assert_eq!(report.descriptors[0].peak_live_count, 2);
        assert_eq!(report.descriptors[0].usage, usage);
        assert_eq!(plan.lifetimes().len(), 4);
        assert_eq!(plan.lifetimes()[3].last_use, 4);
    }

    #[test]
    fn plan_refuses_foreign_image_node() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let mut first = RenderGraph::new();
        let foreign = first.import_image("foreign", size, wgpu::TextureFormat::Rgba8Unorm);
        let mut second = RenderGraph::new();
        let output =
            second.transient_image(plan_desc(size, wgpu::TextureUsages::RENDER_ATTACHMENT));
        let error = second
            .add_plan_task(
                "foreign input",
                vec![ImageUse::sampled_read(foreign)],
                ImageUse::color_attachment(output, ImageLoad::Clear),
                noop_callback(),
            )
            .expect_err("foreign node must be refused");
        assert!(matches!(error, GraphBuildError::ForeignImageNode { .. }));
    }

    #[test]
    fn plan_keeps_sibling_ready_order_deterministic() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let mut graph = RenderGraph::new();
        let input = graph.import_image("input", size, wgpu::TextureFormat::Rgba8Unorm);
        let left = graph.transient_image(plan_desc(size, usage));
        let right = graph.transient_image(plan_desc(size, usage));
        let join = graph.transient_image(plan_desc(size, usage));
        graph
            .add_plan_task(
                "left",
                vec![ImageUse::sampled_read(input)],
                ImageUse::color_attachment(left, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "right",
                vec![ImageUse::sampled_read(input)],
                ImageUse::color_attachment(right, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "join",
                vec![ImageUse::sampled_read(left), ImageUse::sampled_read(right)],
                ImageUse::color_attachment(join, ImageLoad::Clear),
                noop_callback(),
            )
            .unwrap();
        let plan = graph.compile(&[join]).unwrap();
        let dump = plan.dump();
        assert!(dump.find("left").unwrap() < dump.find("right").unwrap());
        assert!(dump.contains("edges=[0->2, 1->2]"));
    }

    #[test]
    fn plan_refuses_incompatible_usage_and_fresh_load() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let mut graph = RenderGraph::new();
        let input = graph.transient_image(plan_desc(size, wgpu::TextureUsages::RENDER_ATTACHMENT));
        let output = graph.transient_image(plan_desc(size, wgpu::TextureUsages::RENDER_ATTACHMENT));
        let error = graph
            .add_plan_task(
                "sample without binding usage",
                vec![ImageUse::sampled_read(input)],
                ImageUse::color_attachment(output, ImageLoad::Clear),
                noop_callback(),
            )
            .expect_err("sampled input must require texture binding usage");
        assert!(matches!(error, GraphBuildError::MissingImageUsage { .. }));

        let mut graph = RenderGraph::new();
        let output = graph.transient_image(plan_desc(
            size,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        ));
        let error = graph
            .add_plan_task(
                "load fresh output",
                Vec::new(),
                ImageUse {
                    image: output,
                    access: ImageAccess::ColorAttachment {
                        load: ImageLoad::Load,
                        store: true,
                    },
                },
                noop_callback(),
            )
            .expect_err("fresh transient output cannot load");
        assert!(matches!(error, GraphBuildError::InvalidOutputLoad { .. }));
    }

    #[test]
    fn imported_metadata_predicate_rejects_size_format_and_usage_mismatches() {
        let expected_size = wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        };
        let actual_size = wgpu::Extent3d {
            width: 8,
            height: 4,
            depth_or_array_layers: 1,
        };
        let required = wgpu::TextureUsages::TEXTURE_BINDING;
        let actual = required | wgpu::TextureUsages::COPY_SRC;
        assert!(imported_image_matches(
            expected_size,
            wgpu::TextureFormat::Rgba8Unorm,
            required,
            expected_size,
            wgpu::TextureFormat::Rgba8Unorm,
            actual,
        ));
        assert!(!imported_image_matches(
            expected_size,
            wgpu::TextureFormat::Rgba8Unorm,
            required,
            actual_size,
            wgpu::TextureFormat::Rgba8Unorm,
            actual,
        ));
        assert!(!imported_image_matches(
            expected_size,
            wgpu::TextureFormat::Rgba8Unorm,
            required,
            expected_size,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            actual,
        ));
        assert!(!imported_image_matches(
            expected_size,
            wgpu::TextureFormat::Rgba8Unorm,
            required,
            expected_size,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::COPY_SRC,
        ));
    }

    #[test]
    fn imported_image_can_be_a_load_preserving_output() {
        let size = wgpu::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        };
        let mut graph = RenderGraph::new();
        let source = graph.import_image("source", size, wgpu::TextureFormat::Rgba8Unorm);
        let target = graph.import_image("target", size, wgpu::TextureFormat::Rgba8Unorm);
        graph
            .add_plan_task(
                "composite imported target",
                vec![ImageUse::sampled_read(source)],
                ImageUse::color_attachment(target, ImageLoad::Load),
                noop_callback(),
            )
            .unwrap();

        let plan = graph.compile(&[target]).unwrap();
        assert_eq!(plan.logical_report().transient_creations, 0);
        let dump = plan.dump();
        assert!(dump.contains("image#0 imported \"source\""));
        assert!(dump.contains("image#1 imported \"target\""));
        assert!(dump.contains("ColorAttachment { load: Load, store: true }"));
    }

    #[test]
    fn imported_outputs_require_load_and_store() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let mut graph = RenderGraph::new();
        let target = graph.import_image("target", size, wgpu::TextureFormat::Rgba8Unorm);
        let error = graph
            .add_plan_task(
                "clear imported target",
                Vec::new(),
                ImageUse::color_attachment(target, ImageLoad::Clear),
                noop_callback(),
            )
            .expect_err("imported targets must preserve their initialized contents");
        assert!(matches!(
            error,
            GraphBuildError::InvalidImportedOutputAccess {
                access: ImageAccess::ColorAttachment {
                    load: ImageLoad::Clear,
                    store: true
                },
                ..
            }
        ));

        let mut graph = RenderGraph::new();
        let target = graph.import_image("target", size, wgpu::TextureFormat::Rgba8Unorm);
        let error = graph
            .add_plan_task(
                "discard imported target",
                Vec::new(),
                ImageUse {
                    image: target,
                    access: ImageAccess::ColorAttachment {
                        load: ImageLoad::Load,
                        store: false,
                    },
                },
                noop_callback(),
            )
            .expect_err("imported targets must store their result");
        assert!(matches!(
            error,
            GraphBuildError::InvalidImportedOutputAccess {
                access: ImageAccess::ColorAttachment {
                    load: ImageLoad::Load,
                    store: false
                },
                ..
            }
        ));
    }

    #[test]
    fn imported_output_duplicate_producers_remain_rejected() {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let mut graph = RenderGraph::new();
        let target = graph.import_image("target", size, wgpu::TextureFormat::Rgba8Unorm);
        graph
            .add_plan_task(
                "first imported producer",
                Vec::new(),
                ImageUse::color_attachment(target, ImageLoad::Load),
                noop_callback(),
            )
            .unwrap();
        graph
            .add_plan_task(
                "second imported producer",
                Vec::new(),
                ImageUse::color_attachment(target, ImageLoad::Load),
                noop_callback(),
            )
            .unwrap();
        assert!(matches!(
            graph.compile(&[target]),
            Err(GraphBuildError::DuplicateProducer { image }) if image == target
        ));
    }
}
