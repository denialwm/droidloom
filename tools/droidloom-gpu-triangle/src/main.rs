//! Spinning triangle for GPU forwarding bring-up.
//!
//! Renders with whatever Vulkan driver the loader gives it (native NVIDIA, or
//! Droidloom's forwarding driver through `VK_DRIVER_FILES`) and shows the
//! frames in the compositor without Vulkan WSI, the way Android frames travel.

mod gpu;
mod window;

use std::time::{Duration, Instant};

use gpu::{Gpu, Placement};
use window::Compositor;

const BUFFERS: usize = 3;

struct Options {
    gpu: String,
    frames: u64,
    headless: bool,
    fps: u32,
    draws: u32,
    check_memory: bool,
    check_target: bool,
    vram: bool,
    width: u32,
    height: u32,
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        gpu: "NVIDIA".to_owned(),
        frames: 0,
        headless: false,
        fps: 0,
        draws: 1,
        check_memory: false,
        check_target: false,
        vram: false,
        width: 960,
        height: 640,
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or(format!("{argument} needs a value"));
        match argument.as_str() {
            "--gpu" => options.gpu = value()?,
            "--frames" => options.frames = value()?.parse().map_err(|_| "bad --frames")?,
            "--headless" => options.headless = true,
            "--check-memory" => options.check_memory = true,
            "--check-target" => options.check_target = true,
            "--vram" => options.vram = true,
            "--fps" => options.fps = value()?.parse().map_err(|_| "bad --fps")?,
            "--draws" => options.draws = value()?.parse().map_err(|_| "bad --draws")?,
            "--size" => {
                let size = value()?;
                let (width, height) = size.split_once('x').ok_or("--size is WIDTHxHEIGHT")?;
                options.width = width.parse().map_err(|_| "bad width")?;
                options.height = height.parse().map_err(|_| "bad height")?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: droidloom-gpu-triangle [--gpu NAME] [--frames N] [--size WxH] [--draws N] [--vram] [--headless [--fps N]] [--check-memory] [--check-target]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn run() -> Result<(), String> {
    let options = parse()?;
    // Frames are LINEAR system memory unless `--vram` puts them in VRAM in
    // the GPU's own layout. Without a window they never leave the GPU,
    // unless one of the two is to be checked.
    let placement = match (options.vram, options.headless && !options.check_target) {
        (true, _) => Placement::Vram,
        (false, true) => Placement::Local,
        (false, false) => Placement::Linear,
    };
    let compositor = if options.headless { None } else { Some(Compositor::connect()?) };
    let accepted = compositor.as_ref().map(Compositor::modifiers);
    let mut gpu = Gpu::new(&options.gpu, options.width, options.height, BUFFERS, placement, accepted)?;
    gpu.draws = options.draws.max(1);
    if options.check_memory {
        eprintln!("{}", gpu.check_memory()?);
    }
    if options.check_target {
        eprintln!("{}", gpu.check_target()?);
    }
    eprintln!(
        "rendering on {} ({}), modifier {:#x}, stride {}",
        gpu.name, gpu.render_node, gpu.targets[0].modifier, gpu.targets[0].stride
    );
    let Some(compositor) = compositor else { return headless(&gpu, &options) };
    let mut window = compositor.open(
        &format!("Triangle on {}", gpu.name),
        options.width,
        options.height,
        &gpu.render_node,
        &gpu.targets,
    )?;

    let started = Instant::now();
    let mut report = Instant::now();
    let mut frames = 0_u64;
    let mut window_frames = 0_u32;
    let mut cpu = Duration::ZERO;
    while !window.closed() && (options.frames == 0 || frames < options.frames) {
        let index = (frames % BUFFERS as u64) as usize;
        let release = window.release_fence(index)?;
        let work = Instant::now();
        let rendered = gpu.render(index, started.elapsed().as_secs_f32() * 1.2, release)?;
        cpu += work.elapsed();
        window.present(index, &rendered)?;
        frames += 1;
        window_frames += 1;
        if report.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "{:.1} fps, render call {:.1} µs",
                f64::from(window_frames) / report.elapsed().as_secs_f64(),
                cpu.as_secs_f64() * 1e6 / f64::from(window_frames)
            );
            report = Instant::now();
            window_frames = 0;
            cpu = Duration::ZERO;
        }
    }
    Ok(())
}

/// Render without a window: no focus taken, no compositor pacing. `--fps`
/// paces frames like a display would, so idle gaps between frames stay.
fn headless(gpu: &Gpu, options: &Options) -> Result<(), String> {
    let period = (options.fps > 0).then(|| Duration::from_secs_f64(1.0 / f64::from(options.fps)));
    let started = Instant::now();
    let mut next = Instant::now();
    let mut report = Instant::now();
    let mut frames = 0_u64;
    let mut window_frames = 0_u32;
    let mut cpu = Duration::ZERO;
    while options.frames == 0 || frames < options.frames {
        let index = (frames % BUFFERS as u64) as usize;
        let work = Instant::now();
        drop(gpu.render(index, started.elapsed().as_secs_f32() * 1.2, None)?);
        cpu += work.elapsed();
        frames += 1;
        window_frames += 1;
        if report.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "{:.1} fps, render call {:.1} µs",
                f64::from(window_frames) / report.elapsed().as_secs_f64(),
                cpu.as_secs_f64() * 1e6 / f64::from(window_frames)
            );
            report = Instant::now();
            window_frames = 0;
            cpu = Duration::ZERO;
        }
        if let Some(period) = period {
            next += period;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-gpu-triangle: {error}");
        std::process::exit(1);
    }
}
