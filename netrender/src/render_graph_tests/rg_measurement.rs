// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Opt-in RG1 repeated-plan measurement.
//!
//! Run with `cargo test -p netrender --lib rg_measurement -- --ignored
//! --nocapture`. This is intentionally ignored: it needs a physical wgpu
//! adapter and is a measurement receipt, not a default correctness test.
//! Each row uses one booted device, 16 warmups, and 64 measured iterations.
//! The measured plan is the same crate-local graph used by
//! `Renderer::build_box_shadow_mask`; the public wrapper is not looped because
//! it retains registered Vello image handles for tile-cache correctness.

use std::time::{Duration, Instant};

use netrender_device::render_graph::ExecutionReport;
use crate::{NetrenderOptions, Renderer, boot};

const WARMUPS: usize = 16;
const SAMPLES: usize = 64;
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_FRAME_BUDGET_MS: f64 = 16.667;

struct Workload {
    name: &'static str,
    dim: u32,
    blur_radius_px: f32,
}

struct Sample {
    construction: Duration,
    binding: Duration,
    execute_overhead: Duration,
    report: ExecutionReport,
    completion: Duration,
}

fn workloads() -> [Workload; 2] {
    [
        Workload {
            name: "small",
            dim: 256,
            blur_radius_px: 16.0,
        },
        Workload {
            name: "large",
            dim: 1024,
            blur_radius_px: 64.0,
        },
    ]
}

fn wait_for_completion(renderer: &Renderer) -> Duration {
    let started = Instant::now();
    renderer
        .wgpu_device
        .core
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(COMPLETION_TIMEOUT),
        })
        .expect("bounded render-graph completion poll");
    started.elapsed()
}

fn run_once(renderer: &Renderer, workload: &Workload) -> (Sample, wgpu::Texture) {
    // The production graph has no imported physical image. `binding` is
    // therefore explicitly zero, rather than pretending that callback bind
    // groups are a separate phase: those are created while encoding each task.
    let graph_started = Instant::now();
    let (plan, output) = renderer.build_box_shadow_plan(
        workload.dim,
        [
            64.0,
            64.0,
            (workload.dim - 64) as f32,
            (workload.dim - 64) as f32,
        ],
        8.0,
        workload.blur_radius_px,
        false,
    );
    let graph_build_inclusive = graph_started.elapsed();
    let binding = Duration::ZERO;

    let execute_started = Instant::now();
    let (texture, report) = renderer.execute_box_shadow_plan(plan, output);
    let execute_elapsed = execute_started.elapsed();
    let accounted = report.allocate_duration + report.encode_duration + report.submit_duration;
    // Residual execute work includes imported-binding validation (empty for
    // this workload), encoder/map setup, and report assembly. Keep it named as
    // an unattributed overhead rather than presenting it as one precise phase.
    let execute_overhead = execute_elapsed.saturating_sub(accounted);
    let completion = wait_for_completion(renderer);
    // Plan compilation happens inside build_box_shadow_plan. Keep it in the
    // explicit compile column, rather than charging it to graph construction.
    let construction = graph_build_inclusive.saturating_sub(report.compile_duration);

    (
        Sample {
            construction,
            binding,
            execute_overhead,
            report,
            completion,
        },
        texture,
    )
}

fn percentile(values: &mut [u128], numerator: usize, denominator: usize) -> Duration {
    values.sort_unstable();
    let index = ((values.len() * numerator).div_ceil(denominator)).saturating_sub(1);
    Duration::from_nanos(values[index] as u64)
}

fn median(values: &[Duration]) -> Duration {
    let mut nanos = values.iter().map(Duration::as_nanos).collect::<Vec<_>>();
    percentile(&mut nanos, 50, 100)
}

fn p95(values: &[Duration]) -> Duration {
    let mut nanos = values.iter().map(Duration::as_nanos).collect::<Vec<_>>();
    percentile(&mut nanos, 95, 100)
}

fn format_duration(duration: Duration) -> String {
    format!("{:.3}ms", duration.as_secs_f64() * 1_000.0)
}

fn print_row(workload: &Workload, samples: &[Sample]) {
    let construction = samples.iter().map(|s| s.construction).collect::<Vec<_>>();
    let binding = samples.iter().map(|s| s.binding).collect::<Vec<_>>();
    let execute_overhead = samples
        .iter()
        .map(|s| s.execute_overhead)
        .collect::<Vec<_>>();
    let compile = samples
        .iter()
        .map(|s| s.report.compile_duration)
        .collect::<Vec<_>>();
    let allocate = samples
        .iter()
        .map(|s| s.report.allocate_duration)
        .collect::<Vec<_>>();
    let encode = samples
        .iter()
        .map(|s| s.report.encode_duration)
        .collect::<Vec<_>>();
    let submit = samples
        .iter()
        .map(|s| s.report.submit_duration)
        .collect::<Vec<_>>();
    let completion = samples.iter().map(|s| s.completion).collect::<Vec<_>>();
    let total_to_completion = samples
        .iter()
        .map(|s| {
            s.construction
                + s.binding
                + s.execute_overhead
                + s.report.compile_duration
                + s.report.allocate_duration
                + s.report.encode_duration
                + s.report.submit_duration
                + s.completion
        })
        .collect::<Vec<_>>();
    let host_work = samples
        .iter()
        .map(|s| {
            s.construction
                + s.binding
                + s.execute_overhead
                + s.report.compile_duration
                + s.report.allocate_duration
                + s.report.encode_duration
                + s.report.submit_duration
        })
        .collect::<Vec<_>>();
    let first = &samples[0].report;

    eprintln!(
        "rg-measurement row={} dim={} blur_radius_px={} warmups={} samples={} completion_timeout={}s",
        workload.name,
        workload.dim,
        workload.blur_radius_px,
        WARMUPS,
        samples.len(),
        COMPLETION_TIMEOUT.as_secs()
    );
    eprintln!(
        "  phase median p95: construction={} {}; binding={} {}; execute_overhead={} {}; compile={} {}; allocate={} {}; encode={} {}; submit={} {}; completion={} {}; total_to_completion={} {}",
        format_duration(median(&construction)),
        format_duration(p95(&construction)),
        format_duration(median(&binding)),
        format_duration(p95(&binding)),
        format_duration(median(&execute_overhead)),
        format_duration(p95(&execute_overhead)),
        format_duration(median(&compile)),
        format_duration(p95(&compile)),
        format_duration(median(&allocate)),
        format_duration(p95(&allocate)),
        format_duration(median(&encode)),
        format_duration(p95(&encode)),
        format_duration(median(&submit)),
        format_duration(p95(&submit)),
        format_duration(median(&completion)),
        format_duration(p95(&completion)),
        format_duration(median(&total_to_completion)),
        format_duration(p95(&total_to_completion)),
    );
    eprintln!(
        "  host_work median={} p95={}; transient_creations={} logical_created_bytes={:?} peak_live_images={} peak_live_bytes={:?}",
        format_duration(median(&host_work)),
        format_duration(p95(&host_work)),
        first.transient_creations,
        first.logical_created_bytes,
        first.peak_live_count,
        first.peak_live_bytes,
    );
    for descriptor in &first.descriptors {
        eprintln!(
            "  descriptor size={}x{} format={:?} usage={:?} creations={} estimated_bytes={:?} projected_peak_live={} projected_peak_bytes={:?}",
            descriptor.size.width,
            descriptor.size.height,
            descriptor.format,
            descriptor.usage,
            descriptor.transient_creations,
            descriptor.estimated_bytes,
            descriptor.peak_live_count,
            descriptor.peak_live_bytes,
        );
    }
    let frame_budget_ms = std::env::var("NETRENDER_RG_FRAME_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_FRAME_BUDGET_MS);
    let allocation_p95_ms = p95(&allocate).as_secs_f64() * 1_000.0;
    let host_work_p95_ms = p95(&host_work).as_secs_f64() * 1_000.0;
    let exact_descriptor_pressure = first.descriptors.iter().any(|descriptor| {
        descriptor.transient_creations >= 3
            && descriptor.peak_live_count >= 1
            && descriptor.transient_creations >= descriptor.peak_live_count.saturating_mul(2)
    });
    let material = exact_descriptor_pressure
        && first.transient_creations >= 3
        && (allocation_p95_ms >= frame_budget_ms * 0.02
            || (allocation_p95_ms >= host_work_p95_ms * 0.15 && allocation_p95_ms >= 0.10));
    eprintln!(
        "  pool_gate frame_budget_ms={:.3} allocation_p95_ms={:.3} host_work_p95_ms={:.3} exact_descriptor_pressure={} verdict={}",
        frame_budget_ms,
        allocation_p95_ms,
        host_work_p95_ms,
        exact_descriptor_pressure,
        if material { "MATERIAL" } else { "NOT_MATERIAL" },
    );
    eprintln!(
        "  methodology: logical peak-live is a projected pooling lower bound; the executor currently eagerly creates all task outputs, so it is not physical live allocation evidence"
    );
}

fn assert_mask_oracle(renderer: &Renderer, output: &wgpu::Texture, dim: u32) {
    let bytes = renderer.wgpu_device.read_rgba8_texture(output, dim, dim);
    let pixel = |x: u32, y: u32| -> [u8; 4] {
        let index = ((y * dim + x) * 4) as usize;
        bytes[index..index + 4].try_into().expect("RGBA pixel")
    };
    assert!(
        pixel(dim / 2, dim / 2)[3] >= 240,
        "box-shadow center should remain covered"
    );
    let outside = pixel(48, dim / 2);
    assert!(
        outside[3] > 5 && outside[3] < 250,
        "box-shadow edge should remain soft, got {outside:?}"
    );
    assert!(
        pixel(0, 0)[3] <= 5,
        "box-shadow far outside should remain clear"
    );
}

#[test]
#[ignore = "physical repeated-plan measurement receipt"]
fn rg1_repeated_box_shadow_measurement() {
    let _gpu_guard = super::gpu_test_guard();
    let handles = boot().expect("wgpu boot");
    let adapter_info = handles.adapter.get_info();
    let renderer = crate::create_netrender_instance(
        handles,
        NetrenderOptions {
            tile_cache_size: Some(64),
            enable_vello: true,
            ..Default::default()
        },
    )
    .expect("create_netrender_instance");
    eprintln!(
        "rg-measurement adapter name={:?} backend={:?} driver={:?} driver_info={:?}",
        adapter_info.name, adapter_info.backend, adapter_info.driver, adapter_info.driver_info
    );

    for workload in workloads() {
        for _ in 0..WARMUPS {
            let (_sample, texture) = run_once(&renderer, &workload);
            drop(texture);
        }

        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let (sample, texture) = run_once(&renderer, &workload);
            // Each iteration has completed before its output is dropped. This
            // keeps the timed loop from retaining physical outputs.
            drop(texture);
            samples.push(sample);
        }

        let first = &samples[0].report;
        assert!(
            first.transient_creations >= 3 && first.peak_live_count >= 2,
            "measurement workload must retain repeated-plan structural pressure"
        );
        print_row(&workload, &samples);
        // Keep the pixel/readback oracle outside the timed sample set.
        let (_oracle_sample, oracle_output) = run_once(&renderer, &workload);
        assert_mask_oracle(&renderer, &oracle_output, workload.dim);
        drop(oracle_output);
    }
}
