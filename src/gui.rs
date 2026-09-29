//! The graphical front end: pick an image, pick a card (or an output file), write, verify.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Vec2};
use egui_file_dialog::FileDialog;

use rockchip_sd_tool::disks::{self, DiskInfo};
use rockchip_sd_tool::job::JobSpec;
use rockchip_sd_tool::rkfw::{self, RkfwImage};
use rockchip_sd_tool::util::{human_bytes, parse_size, vendor_gb};
use rockchip_sd_tool::writer::{Cancel, Progress};
use rockchip_sd_tool::{elevate, gpt, plan};

const ACCENT: Color32 = Color32::from_rgb(0xc5, 0x1a, 0x4a);
const OK_GREEN: Color32 = Color32::from_rgb(0x2e, 0x9e, 0x5b);

/// Common card sizes offered for file output. Values are the smallest capacity commonly found
/// for that class, so an image made for the preset fits every card sold under that name.
const CARD_PRESETS: &[(&str, u64)] = &[
    ("8 GB", 7_800_000_000),
    ("16 GB", 15_500_000_000),
    ("32 GB", 31_000_000_000),
    ("64 GB", 62_000_000_000),
    ("128 GB", 124_000_000_000),
    ("256 GB", 249_000_000_000),
    ("512 GB", 498_000_000_000),
    ("1 TB", 996_000_000_000),
];

pub fn run(preload: Option<PathBuf>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rockchip SD Tool")
            .with_inner_size(Vec2::new(760.0, 520.0))
            .with_min_inner_size(Vec2::new(640.0, 440.0))
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Rockchip SD Tool",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            if let Some(p) = preload {
                app.load_image(p);
            }
            Ok(Box::new(app))
        }),
    )
        .map_err(|e| anyhow::anyhow!("cannot start the window system: {e}"))
}

fn icon() -> egui::IconData {
    // A simple generated icon: a rounded card shape with a notch.
    let n = 64usize;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let inside = x >= 14 && x < 50 && y >= 6 && y < 58 && !(x >= 42 && y < 14);
            let stripe = inside && y >= 40 && y < 46 && x >= 20 && x < 44;
            let i = (y * n + x) * 4;
            if stripe {
                rgba[i..i + 4].copy_from_slice(&[255, 215, 0, 255]);
            } else if inside {
                rgba[i..i + 4].copy_from_slice(&[ACCENT.r(), ACCENT.g(), ACCENT.b(), 255]);
            }
        }
    }
    egui::IconData { rgba, width: n as u32, height: n as u32 }
}

struct ImageInfo {
    path: PathBuf,
    name: String,
    model: String,
    chip: String,
    version: String,
    built: String,
    min_bytes: u64,
    partitions: Vec<String>,
    md5: Option<String>,
}

impl ImageInfo {
    fn load(path: PathBuf) -> Result<ImageInfo> {
        let img = RkfwImage::open(&path)?;
        // Build a plan for the minimum size to surface layout errors early.
        let min_sectors = gpt::minimum_sectors(&img.parameter);
        plan::build(&img, min_sectors.max(1 << 21))?;
        let partitions = img
            .parameter
            .partitions
            .iter()
            .map(|p| match p.size {
                Some(s) => format!("{}  {}", p.name, human_bytes(s * 512)),
                None => format!("{}  (rest of the card)", p.name),
            })
            .collect();
        Ok(ImageInfo {
            name: path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            model: img.af.model.trim().to_string(),
            chip: rkfw::chip_name(img.header.chip),
            version: rkfw::format_version(img.header.version),
            built: img.header.time.to_string(),
            min_bytes: min_sectors * 512,
            partitions,
            md5: img.md5_hex.clone(),
            path,
        })
    }
}

#[derive(Clone)]
enum Storage {
    Device(DiskInfo),
    File { path: PathBuf, size: u64 },
}

impl Storage {
    fn label(&self) -> String {
        match self {
            Storage::Device(d) => d.label(),
            Storage::File { path, size } => format!(
                "{} ({} card)",
                path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                vendor_gb(*size)
            ),
        }
    }
    fn size(&self) -> u64 {
        match self {
            Storage::Device(d) => d.size,
            Storage::File { size, .. } => *size,
        }
    }
}

enum JobHandle {
    InProcess { rx: Receiver<Progress>, done: Receiver<Result<()>>, cancel: Cancel },
    Helper { child: std::process::Child, progress_file: PathBuf, read: usize, finished: Option<Result<()>> },
}

struct Job {
    handle: JobHandle,
    progress: Progress,
    started: Instant,
    target_desc: String,
    is_device: bool,
    is_upgrade: bool,
}

#[derive(PartialEq)]
enum Popup {
    None,
    Storage,
    Confirm,
}

struct App {
    image: Option<ImageInfo>,
    image_error: Option<String>,
    storage: Option<Storage>,
    verify: bool,
    upgrade: bool,
    popup: Popup,
    disks: Vec<DiskInfo>,
    disks_refreshed: Instant,
    show_all: bool,
    file_mode: bool,
    file_path_text: String,
    preset: usize,
    custom_size: String,
    image_dialog: FileDialog,
    out_dialog: FileDialog,
    job: Option<Job>,
    outcome: Option<(bool, String)>,
    details_open: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> App {
        cc.egui_ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 8.0);
            style.spacing.button_padding = Vec2::new(14.0, 8.0);
        });
        App {
            image: None,
            image_error: None,
            storage: None,
            verify: true,
            upgrade: false,
            popup: Popup::None,
            disks: disks::list(),
            disks_refreshed: Instant::now(),
            show_all: false,
            file_mode: false,
            file_path_text: String::new(),
            preset: 3,
            custom_size: String::new(),
            image_dialog: FileDialog::new()
                .title("Choose a Rockchip firmware image")
                .add_file_filter_extensions("Firmware images", vec!["img", "IMG"]),
            out_dialog: FileDialog::new()
                .title("Save card image as")
                .add_save_extension("Raw image (.img)", "img")
                .add_save_extension("Compressed image (.img.xz)", "img.xz")
                .default_save_extension("Compressed image (.img.xz)")
                .default_file_name("sdcard.img.xz"),
            job: None,
            outcome: None,
            details_open: false,
        }
    }

    fn load_image(&mut self, p: PathBuf) {
        match ImageInfo::load(p) {
            Ok(i) => {
                self.image = Some(i);
                self.image_error = None;
            }
            Err(e) => {
                self.image = None;
                self.image_error = Some(format!("{e:#}"));
            }
        }
        self.outcome = None;
    }

    fn file_size(&self) -> Option<u64> {
        if self.custom_size.trim().is_empty() {
            Some(CARD_PRESETS[self.preset].1)
        } else {
            parse_size(&self.custom_size)
        }
    }

    fn start_job(&mut self) {
        let (Some(img), Some(storage)) = (&self.image, &self.storage) else { return };
        self.outcome = None;
        let (output, size, is_device) = match storage {
            Storage::Device(d) => (d.path.clone(), None, true),
            Storage::File { path, size } => (path.to_string_lossy().to_string(), Some(*size), false),
        };
        let size = if self.upgrade { None } else { size };
        let spec = JobSpec {
            image: img.path.clone(),
            output,
            size,
            verify: self.verify,
            xz_level: 3,
            verify_blocks: true,
            upgrade: self.upgrade,
        };
        let target_desc = storage.label();
        let use_helper = is_device && elevate::needs_helper();
        let handle = if use_helper {
            let dir = match elevate::job_dir() {
                Ok(d) => d,
                Err(e) => {
                    self.outcome = Some((false, format!("{e:#}")));
                    return;
                }
            };
            let progress_file = dir.join("progress");
            match elevate::spawn_helper(&spec, &progress_file) {
                Ok(child) => JobHandle::Helper { child, progress_file, read: 0, finished: None },
                Err(e) => {
                    self.outcome = Some((false, format!("Cannot start the privileged writer: {e:#}")));
                    return;
                }
            }
        } else {
            let (tx, rx): (Sender<Progress>, Receiver<Progress>) = std::sync::mpsc::channel();
            let (dtx, done) = std::sync::mpsc::channel();
            let cancel = Cancel::new();
            let cflag = Arc::clone(&cancel.0);
            std::thread::spawn(move || {
                let cancel = Cancel(cflag);
                let mut last = Instant::now();
                let mut last_step = String::new();
                let mut progress = |p: Progress| {
                    let now = Instant::now();
                    if now.duration_since(last).as_millis() >= 50 || p.step != last_step || p.done == p.total {
                        last = now;
                        last_step = p.step.clone();
                        let _ = tx.send(p);
                    }
                };
                let r = rockchip_sd_tool::job::run(&spec, &mut progress, &cancel).map(|_| ());
                let _ = dtx.send(r);
            });
            JobHandle::InProcess { rx, done, cancel }
        };
        self.job = Some(Job {
            handle,
            progress: Progress { phase: "write".into(), step: "Starting".into(), done: 0, total: 1, retries: 0 },
            started: Instant::now(),
            target_desc,
            is_device,
            is_upgrade: self.upgrade,
        });
    }

    fn poll_job(&mut self) {
        let Some(job) = self.job.as_mut() else { return };
        let mut finished: Option<Result<()>> = None;
        match &mut job.handle {
            JobHandle::InProcess { rx, done, .. } => {
                while let Ok(p) = rx.try_recv() {
                    job.progress = p;
                }
                if let Ok(r) = done.try_recv() {
                    finished = Some(r);
                }
            }
            JobHandle::Helper { child, progress_file, read, finished: fin } => {
                if let Ok(data) = std::fs::read(&*progress_file) {
                    if data.len() > *read {
                        let new = &data[*read..];
                        // Only consume complete lines.
                        if let Some(nl) = new.iter().rposition(|&c| c == b'\n') {
                            let text = String::from_utf8_lossy(&new[..=nl]).to_string();
                            *read += nl + 1;
                            for line in text.lines() {
                                if let Ok(p) = serde_json::from_str::<Progress>(line) {
                                    job.progress = p;
                                } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                                    if v.get("result").is_some() {
                                        *fin = Some(Ok(()));
                                    } else if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                                        *fin = Some(Err(anyhow::anyhow!("{e}")));
                                    }
                                }
                            }
                        }
                    }
                }
                if let Ok(Some(status)) = child.try_wait() {
                    let r = match fin.take() {
                        Some(r) => r,
                        None => {
                            let mut err = String::new();
                            if let Some(mut e) = child.stderr.take() {
                                use std::io::Read;
                                let _ = e.read_to_string(&mut err);
                            }
                            let err = err.trim().to_string();
                            if status.success() {
                                Ok(())
                            } else if err.is_empty() {
                                Err(anyhow::anyhow!("the privileged writer stopped (authentication cancelled or failed)"))
                            } else {
                                Err(anyhow::anyhow!("{err}"))
                            }
                        }
                    };
                    let _ = std::fs::remove_file(&*progress_file);
                    let _ = std::fs::remove_file(elevate::cancel_path(progress_file));
                    if let Some(dir) = progress_file.parent() {
                        let _ = std::fs::remove_dir(dir);
                    }
                    finished = Some(r);
                }
            }
        }
        if let Some(r) = finished {
            let elapsed = job.started.elapsed();
            let is_device = job.is_device;
            let was_upgrade = job.is_upgrade;
            let desc = job.target_desc.clone();
            self.job = None;
            self.outcome = Some(match r {
                Ok(()) => (
                    true,
                    match (is_device, was_upgrade) {
                        (true, true) => format!("Upgrade successful. {} is ready and its user data was kept; you can remove the card now. ({})", desc, fmt_dur(elapsed)),
                        (true, false) => format!("Write successful. {} is ready; you can remove the card now. ({})", desc, fmt_dur(elapsed)),
                        (false, true) => format!("{} upgraded. ({})", desc, fmt_dur(elapsed)),
                        (false, false) => format!("Image written to {}. ({})", desc, fmt_dur(elapsed)),
                    },
                ),
                Err(e) => (false, format!("{e:#}")),
            });
            if is_device {
                self.disks = disks::list();
            }
        }
    }

    fn cancel_job(&mut self) {
        if let Some(job) = self.job.as_mut() {
            match &mut job.handle {
                JobHandle::InProcess { cancel, .. } => cancel.0.store(true, std::sync::atomic::Ordering::Relaxed),
                JobHandle::Helper { progress_file, .. } => {
                    let _ = std::fs::write(elevate::cancel_path(progress_file), b"cancel");
                }
            }
        }
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.poll_job();
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.popup == Popup::Storage && self.disks_refreshed.elapsed() > Duration::from_secs(2) {
            self.disks = disks::list();
            self.disks_refreshed = Instant::now();
            ctx.request_repaint_after(Duration::from_secs(2));
        }

        // File dialogs.
        self.image_dialog.update(ctx);
        if let Some(p) = self.image_dialog.take_picked() {
            self.load_image(p);
        }
        self.out_dialog.update(ctx);
        if let Some(p) = self.out_dialog.take_picked() {
            self.file_path_text = p.to_string_lossy().to_string();
        }
        // Drag and drop of an image file onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if let Some(p) = dropped.into_iter().next() {
            if self.job.is_none() {
                self.load_image(p);
            }
        }

        let busy = self.job.is_some();

        egui::Panel::top("header").frame(egui::Frame::NONE.fill(ACCENT).inner_margin(egui::Margin::symmetric(20, 14))).show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Rockchip SD Tool").size(26.0).strong().color(Color32::WHITE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("SD boot cards from RKFW firmware").color(Color32::from_white_alpha(200)));
                });
            });
        });

        egui::CentralPanel::default_margins().frame(egui::Frame::central_panel(&root.style()).inner_margin(egui::Margin::same(24))).show(root, |ui| {
            ui.add_space(8.0);
            ui.columns(3, |cols| {
                // Image column.
                cols[0].vertical_centered(|ui| {
                    ui.label(RichText::new("Firmware image").strong().size(15.0));
                    ui.add_space(6.0);
                    let text = match &self.image {
                        Some(i) => i.name.clone(),
                        None => "CHOOSE IMAGE".into(),
                    };
                    if ui.add_enabled(!busy, big_button(&text)).clicked() {
                        self.image_dialog.pick_file();
                    }
                    if let Some(i) = &self.image {
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("{}  {}", i.model, i.chip)).small());
                        ui.label(RichText::new(format!("v{}  {}", i.version, i.built)).small().weak());
                        ui.label(RichText::new(format!("needs a card of {} or more", human_bytes(i.min_bytes))).small().weak());
                    }
                    if let Some(e) = &self.image_error {
                        ui.add_space(4.0);
                        ui.label(RichText::new(e).small().color(ACCENT));
                    }
                });
                // Storage column.
                cols[1].vertical_centered(|ui| {
                    ui.label(RichText::new("Storage").strong().size(15.0));
                    ui.add_space(6.0);
                    let text = match &self.storage {
                        Some(s) => s.label(),
                        None => "CHOOSE STORAGE".into(),
                    };
                    if ui.add_enabled(!busy && self.image.is_some(), big_button(&text)).clicked() {
                        self.disks = disks::list();
                        self.disks_refreshed = Instant::now();
                        self.popup = Popup::Storage;
                    }
                    if let (Some(s), Some(i)) = (&self.storage, &self.image) {
                        if !self.upgrade && s.size() < i.min_bytes {
                            ui.add_space(4.0);
                            ui.label(RichText::new(format!("too small: {} available, {} needed", human_bytes(s.size()), human_bytes(i.min_bytes))).small().color(ACCENT));
                        }
                    }
                });
                // Write column.
                cols[2].vertical_centered(|ui| {
                    ui.label(RichText::new("Write").strong().size(15.0));
                    ui.add_space(6.0);
                    // An upgrade uses the card's own size and layout, so the size of a chosen
                    // image file says nothing about whether it fits.
                    let ready = self.image.is_some()
                        && self
                            .storage
                            .as_ref()
                            .map(|s| self.upgrade || s.size() >= self.image.as_ref().unwrap().min_bytes)
                            .unwrap_or(false);
                    if busy {
                        if ui.add(big_button("CANCEL")).clicked() {
                            self.cancel_job();
                        }
                    } else if ui.add_enabled(ready, big_button("WRITE")).clicked() {
                        match &self.storage {
                            Some(Storage::Device(_)) => self.popup = Popup::Confirm,
                            Some(Storage::File { .. }) => self.start_job(),
                            None => {}
                        }
                    }
                    ui.add_space(4.0);
                    ui.add_enabled(!busy, egui::Checkbox::new(&mut self.verify, "Verify after writing"))
                        .on_hover_text("Every block is already read back and compared right after it is written (and rewritten up to 3 times on mismatch). This adds a second full pass over the card at the end.");
                    ui.add_enabled(!busy, egui::Checkbox::new(&mut self.upgrade, "Upgrade, keep user data"))
                        .on_hover_text(
                            "Writes the loader and every partition this image carries onto a card that already has the same layout, and leaves the partition table and everything else, including user data, untouched. The write stops before it starts if the card's layout does not match.",
                        );
                });
            });

            ui.add_space(18.0);
            ui.separator();
            ui.add_space(10.0);

            if let Some(job) = &self.job {
                let p = &job.progress;
                let frac = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
                let phase = if p.phase == "verify" { "Verifying" } else if job.is_upgrade { "Upgrading" } else { "Writing" };
                ui.label(RichText::new(format!("{phase} {} to {}", p.step, job.target_desc)).strong());
                ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                let elapsed = job.started.elapsed().as_secs_f64();
                let rate = if elapsed > 0.5 { p.done as f64 / elapsed } else { 0.0 };
                let mut line = format!("{} of {}   {}/s", human_bytes(p.done), human_bytes(p.total), human_bytes(rate as u64));
                if p.retries > 0 {
                    line.push_str(&format!("   {} block(s) had to be rewritten", p.retries));
                }
                ui.label(RichText::new(line).weak());
            } else if let Some((ok, msg)) = &self.outcome {
                let color = if *ok { OK_GREEN } else { ACCENT };
                ui.label(RichText::new(msg).color(color).strong());
            } else {
                ui.label(RichText::new("Pick a Rockchip RKFW firmware image (.img), then the SD card to write it to.").weak());
                ui.label(RichText::new("You can also write the card image to a file (.img or .img.xz) for a card of a given size.").weak());
            }

            if let Some(i) = &self.image {
                ui.add_space(10.0);
                egui::CollapsingHeader::new("Image details").default_open(self.details_open).show(ui, |ui| {
                    ui.label(format!("File: {}", i.path.display()));
                    if let Some(m) = &i.md5 {
                        ui.label(format!("MD5 (stored): {m}"));
                    }
                    ui.label("Partitions:");
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for p in &i.partitions {
                            ui.label(RichText::new(p).monospace().small());
                        }
                    });
                });
            }
        });

        self.storage_popup(ctx);
        self.confirm_popup(ctx);
    }
}

fn big_button(text: &str) -> egui::Button<'static> {
    let t = RichText::new(text.to_string()).size(15.0).strong();
    egui::Button::new(t).min_size(Vec2::new(180.0, 44.0)).fill(ACCENT).stroke(egui::Stroke::NONE)
}

impl App {
    fn storage_popup(&mut self, ctx: &egui::Context) {
        if self.popup != Popup::Storage {
            return;
        }
        let mut open = true;
        let mut close = false;
        egui::Window::new("Choose storage")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(Vec2::new(560.0, 420.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.file_mode, false, "SD card / USB reader");
                    ui.selectable_value(&mut self.file_mode, true, "Image file (.img / .img.xz)");
                });
                ui.separator();
                if !self.file_mode {
                    ui.checkbox(&mut self.show_all, "Show all disks (including internal drives; be careful)");
                    let list: Vec<DiskInfo> = self.disks.iter().filter(|d| self.show_all || d.is_candidate()).cloned().collect();
                    if list.is_empty() {
                        ui.add_space(10.0);
                        ui.label(RichText::new("No removable disk found. Insert the SD card and it will appear here.").weak());
                    }
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for d in list {
                            let mut text = format!("{}    {}    [{}]", d.label(), human_bytes(d.size), d.bus);
                            if d.system {
                                text.push_str("    SYSTEM DISK");
                            }
                            if !d.mounts.is_empty() {
                                text.push_str(&format!("    mounted: {}", d.mounts.join(", ")));
                            }
                            let resp = ui.add_sized(
                                Vec2::new(ui.available_width(), 36.0),
                                egui::Button::new(RichText::new(text).color(if d.system { ACCENT } else { ui.visuals().text_color() })),
                            );
                            if resp.clicked() && !d.system {
                                self.storage = Some(Storage::Device(d.clone()));
                                close = true;
                            }
                        }
                    });
                } else {
                    ui.label("Size of the SD card the image is meant for:");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("preset")
                            .selected_text(CARD_PRESETS[self.preset].0)
                            .show_ui(ui, |ui| {
                                for (i, (name, _)) in CARD_PRESETS.iter().enumerate() {
                                    ui.selectable_value(&mut self.preset, i, *name);
                                }
                            });
                        ui.label("or exact size:");
                        ui.add(egui::TextEdit::singleline(&mut self.custom_size).desired_width(150.0).hint_text("e.g. 128177930240 or 250347520s"));
                    });
                    ui.label(RichText::new("Presets use the smallest capacity common for that card class, so the image fits any card of that size. For an exact match enter the card size in bytes, GB, or sectors (s).").small().weak());
                    match self.file_size() {
                        Some(s) => ui.label(format!("Card size: {} ({} bytes, {} sectors)", vendor_gb(s), s, s / 512)),
                        None => ui.label(RichText::new("Enter a valid size").color(ACCENT)),
                    };
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Output file:");
                        ui.add(egui::TextEdit::singleline(&mut self.file_path_text).desired_width(300.0).hint_text("path ending in .img or .img.xz"));
                        if ui.button("Browse...").clicked() {
                            self.out_dialog.save_file();
                        }
                    });
                    ui.label(RichText::new("A .img.xz keeps the file small (zero areas compress to nothing). A raw .img is created as a sparse file of the full card size.").small().weak());
                    ui.add_space(6.0);
                    let ok = !self.file_path_text.trim().is_empty() && self.file_size().is_some();
                    if ui.add_enabled(ok, egui::Button::new("Use this file")).clicked() {
                        let size = self.file_size().unwrap() / 512 * 512;
                        self.storage = Some(Storage::File { path: PathBuf::from(self.file_path_text.trim()), size });
                        close = true;
                    }
                }
            });
        if !open || close {
            self.popup = Popup::None;
        }
    }

    fn confirm_popup(&mut self, ctx: &egui::Context) {
        if self.popup != Popup::Confirm {
            return;
        }
        let Some(Storage::Device(d)) = self.storage.clone() else {
            self.popup = Popup::None;
            return;
        };
        let mut open = true;
        let mut decided = None;
        let title = if self.upgrade { "Upgrade this card?" } else { "Erase and write?" };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                if self.upgrade {
                    ui.label(RichText::new(format!("Every partition this image carries will be replaced on {}.", d.label())).strong());
                    ui.label("The partition table and user data are kept. The card must already have this image's layout; it is checked before anything is written.");
                } else {
                    ui.label(RichText::new(format!("All existing data on {} will be erased.", d.label())).strong());
                }
                if !d.mounts.is_empty() {
                    ui.label(format!("It is currently mounted at {}; it will be unmounted first.", d.mounts.join(", ")));
                }
                if !elevate::is_privileged() {
                    ui.label(RichText::new("You will be asked for your password to write to the card.").weak());
                }
                if cfg!(target_os = "macos") {
                    ui.label(RichText::new("macOS may also ask to allow access to removable volumes; allow it.").weak());
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let go = if self.upgrade { "Yes, upgrade" } else { "Yes, erase and write" };
                    if ui.add(egui::Button::new(RichText::new(go).color(Color32::WHITE)).fill(ACCENT)).clicked() {
                        decided = Some(true);
                    }
                    if ui.button("No").clicked() {
                        decided = Some(false);
                    }
                });
            });
        match decided {
            Some(true) => {
                self.popup = Popup::None;
                self.start_job();
            }
            Some(false) => self.popup = Popup::None,
            None => {
                if !open {
                    self.popup = Popup::None;
                }
            }
        }
    }
}
