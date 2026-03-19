use anyhow::{Context, Result};
use clap::Parser;
use inotify::{EventMask, Inotify, WatchMask};
use log::{debug, error, info, warn};
use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(author, version, about = "On-demand camera daemon for Intel IPU6")]
struct Args {
    #[arg(short, long, default_value = "/dev/video0")]
    device: String,

    #[arg(short, long, default_value = "1920")]
    width: u32,

    #[arg(short = 'H', long, default_value = "1080")]
    height: u32,

    #[arg(short, long, default_value = "30")]
    framerate: u32,

    #[arg(short, long, default_value = "5000")]
    cooldown_ms: u64,

    #[arg(short, long, default_value = "7")]
    buffer_count: u32,

    #[arg(long, default_value = "1000")]
    activation_delay_ms: u64,
}

fn count_device_clients(device: &str, exclude_pids: &[u32]) -> usize {
    let dev_path = Path::new(device);
    let mut count = 0;

    let Ok(proc_dir) = fs::read_dir("/proc") else {
        return 0;
    };

    for entry in proc_dir.flatten() {
        let pid = entry.file_name();
        let pid_str = pid.to_string_lossy();

        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let pid_num: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if exclude_pids.contains(&pid_num) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };

        for fd_entry in fds.flatten() {
            if let Ok(link) = fs::read_link(fd_entry.path()) {
                if link == dev_path {
                    count += 1;
                    break;
                }
            }
        }
    }

    count
}

fn is_process_alive(child: &mut Child) -> bool {
    match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) => true,
        Err(_) => false,
    }
}

struct CameraDaemon {
    args: Args,
    placeholder_process: Option<Child>,
    camera_process: Option<Child>,
    cooldown_start: Option<Instant>,
    running: Arc<AtomicBool>,
    last_client_count: usize,
    camera_active: bool,
    camera_start_time: Option<Instant>,
    inotify: Inotify,
    last_placeholder_attempt: Option<Instant>,
    client_detected_at: Option<Instant>,
}

impl CameraDaemon {
    fn new(args: Args) -> Result<Self> {
        let inotify = Inotify::init().context("Failed to initialize inotify")?;

        inotify
            .watches()
            .add(&args.device, WatchMask::OPEN | WatchMask::CLOSE)
            .context("Failed to add inotify watch")?;

        Ok(Self {
            args,
            placeholder_process: None,
            camera_process: None,
            cooldown_start: None,
            running: Arc::new(AtomicBool::new(true)),
            last_client_count: 0,
            camera_active: false,
            camera_start_time: None,
            inotify,
            last_placeholder_attempt: None,
            client_detected_at: None,
        })
    }

    fn our_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        if let Some(ref p) = self.placeholder_process {
            pids.push(p.id());
        }
        if let Some(ref p) = self.camera_process {
            pids.push(p.id());
        }
        pids
    }

    fn placeholder_alive(&mut self) -> bool {
        if let Some(ref mut child) = self.placeholder_process {
            if is_process_alive(child) {
                return true;
            }
            debug!("Placeholder process exited");
            self.placeholder_process = None;
        }
        false
    }

    fn camera_alive(&mut self) -> bool {
        if let Some(ref mut child) = self.camera_process {
            if is_process_alive(child) {
                return true;
            }
            debug!("Camera process exited");
            self.camera_process = None;
            self.camera_active = false;
            self.camera_start_time = None;
        }
        false
    }

    fn start_placeholder(&mut self) -> Result<()> {
        if let Some(last) = self.last_placeholder_attempt {
            if last.elapsed() < Duration::from_secs(2) {
                return Ok(());
            }
        }
        self.last_placeholder_attempt = Some(Instant::now());

        if self.placeholder_alive() {
            return Ok(());
        }

        info!("Starting placeholder stream (black)");

        let pipeline = format!(
            "videotestsrc pattern=black is-live=true ! \
             video/x-raw,format=YUY2,width={},height={},framerate={}/1 ! \
             v4l2sink device={}",
            self.args.width, self.args.height, self.args.framerate, self.args.device
        );

        let mut child = Command::new("sh")
            .args(["-c", &format!("exec gst-launch-1.0 {}", pipeline)])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn placeholder pipeline")?;

        info!("Placeholder started (PID: {})", child.id());

        std::thread::sleep(Duration::from_millis(500));

        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(mut stderr) = child.stderr.take() {
                    let mut err_output = String::new();
                    let _ = stderr.read_to_string(&mut err_output);
                    if !err_output.is_empty() {
                        warn!("Placeholder stderr: {}", err_output.trim());
                    }
                }
                warn!("Placeholder exited with status: {}", status);
                return Ok(());
            }
            Ok(None) => {
                info!("Placeholder running");
                self.placeholder_process = Some(child);
            }
            Err(e) => {
                warn!("Error checking placeholder: {}", e);
                return Ok(());
            }
        }

        Ok(())
    }

    fn stop_placeholder(&mut self) {
        if let Some(mut child) = self.placeholder_process.take() {
            debug!("Stopping placeholder (PID: {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn start_camera(&mut self) -> Result<()> {
        if self.camera_alive() {
            return Ok(());
        }

        self.stop_placeholder();

        std::thread::sleep(Duration::from_millis(100));

        info!("Starting camera pipeline");

        let pipeline = format!(
            "icamerasrc buffer-count={} ! \
             video/x-raw,format=NV12,width={},height={},framerate={}/1 ! \
             videoconvert ! \
             video/x-raw,format=YUY2 ! \
             v4l2sink device={}",
            self.args.buffer_count,
            self.args.width,
            self.args.height,
            self.args.framerate,
            self.args.device
        );

        let mut child = Command::new("sh")
            .args(["-c", &format!("exec gst-launch-1.0 {}", pipeline)])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn camera pipeline")?;

        info!("Camera started (PID: {})", child.id());

        std::thread::sleep(Duration::from_millis(500));

        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(mut stderr) = child.stderr.take() {
                    let mut err_output = String::new();
                    let _ = stderr.read_to_string(&mut err_output);
                    if !err_output.is_empty() {
                        warn!("Camera stderr: {}", err_output.trim());
                    }
                }
                warn!(
                    "Camera exited with status: {}, restarting placeholder",
                    status
                );
                self.camera_active = false;
                self.start_placeholder()?;
            }
            Ok(None) => {
                info!("Camera running");
                self.camera_process = Some(child);
                self.camera_active = true;
                self.camera_start_time = Some(Instant::now());
                self.cooldown_start = None;
            }
            Err(e) => {
                warn!("Error checking camera: {}", e);
                self.camera_active = false;
                self.start_placeholder()?;
            }
        }

        Ok(())
    }

    fn stop_camera(&mut self) -> Result<()> {
        if let Some(mut child) = self.camera_process.take() {
            info!("Stopping camera (PID: {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        self.camera_active = false;
        self.camera_start_time = None;
        self.cooldown_start = None;

        std::thread::sleep(Duration::from_millis(100));
        self.start_placeholder()?;

        self.clear_inotify_events();

        Ok(())
    }

    fn clear_inotify_events(&mut self) {
        let mut buffer = [0; 1024];
        let _ = self.inotify.read_events(&mut buffer);
    }

    fn wait_and_check_inotify(&mut self, timeout_ms: i32) -> bool {
        let fd = self.inotify.as_raw_fd();
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };

        if ret > 0 && (pollfd.revents & libc::POLLIN) != 0 {
            let mut buffer = [0; 1024];
            let mut had_open = false;

            if let Ok(events) = self.inotify.read_events(&mut buffer) {
                for event in events {
                    if event.mask.contains(EventMask::OPEN) {
                        debug!("Device opened (inotify)");
                        had_open = true;
                    }
                }
            }
            return had_open;
        }

        false
    }

    fn update(&mut self, had_open: bool) -> Result<()> {
        let placeholder_ok = self.placeholder_alive();
        let camera_ok = self.camera_alive();

        if !camera_ok && !placeholder_ok {
            self.camera_active = false;
            self.start_placeholder()?;
        }

        // Only scan /proc when there's activity or camera is active
        let client_count = if had_open || self.camera_active || self.client_detected_at.is_some() {
            let our_pids = self.our_pids();
            count_device_clients(&self.args.device, &our_pids)
        } else {
            0
        };

        if client_count != self.last_client_count {
            info!("Client count: {}", client_count);
            self.last_client_count = client_count;
        }

        let device_opened = had_open && placeholder_ok && self.placeholder_process.is_some();

        if (client_count > 0 || device_opened) && !self.camera_active {
            if self.client_detected_at.is_none() {
                debug!("Client detected, starting activation delay");
                self.client_detected_at = Some(Instant::now());
            }
        } else if client_count == 0 && !self.camera_active {
            self.client_detected_at = None;
        }

        let delay_elapsed = self
            .client_detected_at
            .map(|t| t.elapsed() >= Duration::from_millis(self.args.activation_delay_ms))
            .unwrap_or(false);

        let should_activate = if self.camera_active {
            client_count > 0
        } else {
            delay_elapsed && client_count > 0
        };

        if should_activate {
            self.cooldown_start = None;

            if !self.camera_active {
                info!("Activation delay passed, starting camera");
                self.client_detected_at = None;
                self.start_camera()?;
            }
        } else if self.camera_active && client_count == 0 {
            match self.cooldown_start {
                None => {
                    let min_runtime = Duration::from_millis(2000);
                    let met_min = self
                        .camera_start_time
                        .map(|t| t.elapsed() >= min_runtime)
                        .unwrap_or(true);

                    if met_min {
                        info!("No clients, entering cooldown");
                        self.cooldown_start = Some(Instant::now());
                    }
                }
                Some(start) => {
                    if start.elapsed() >= Duration::from_millis(self.args.cooldown_ms) {
                        info!("Cooldown expired, switching to placeholder");
                        self.stop_camera()?;
                    }
                }
            }
        }

        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        info!("Camera daemon starting");
        info!(
            "Device: {}, Resolution: {}x{}@{}fps, Cooldown: {}ms",
            self.args.device,
            self.args.width,
            self.args.height,
            self.args.framerate,
            self.args.cooldown_ms
        );

        self.start_placeholder()?;

        self.clear_inotify_events();

        let running = self.running.clone();
        ctrlc::set_handler(move || {
            info!("Received shutdown signal");
            running.store(false, Ordering::SeqCst);
        })?;

        info!("Daemon ready, monitoring {}", self.args.device);

        let mut last_update = Instant::now();

        while self.running.load(Ordering::Relaxed) {
            let timeout_ms = if self.camera_active || self.client_detected_at.is_some() {
                200
            } else {
                2000
            };

            let had_open = self.wait_and_check_inotify(timeout_ms);

            let min_interval = if self.camera_active { 200 } else { 500 };
            if !had_open && last_update.elapsed() < Duration::from_millis(min_interval) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            last_update = Instant::now();

            if let Err(e) = self.update(had_open) {
                error!("Update error: {}", e);
            }
        }

        self.stop_placeholder();
        if self.camera_active {
            let _ = self.stop_camera();
        }

        info!("Camera daemon stopped");
        Ok(())
    }
}

impl Drop for CameraDaemon {
    fn drop(&mut self) {
        self.stop_placeholder();
        if let Some(mut child) = self.camera_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let mut daemon = CameraDaemon::new(args)?;

    daemon.run()
}
