use chrono::Local;
use eframe::egui;
use egui::{FontDefinitions, FontFamily, FontId};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Default)]
struct AppConfig {
    output_dir: String,
}

struct VideoProcessor {
    // 文件参数
    source_paths: Vec<String>,
    output_dir: String,
    output_template: String,
    config_path: String,

    // 处理参数
    start_time: String,
    end_time: String,
    rotation: i32,

    // 状态管理
    batch_queue: Vec<BatchTask>,
    processing: bool,
    state: ProcessingState,

    // 新增预览相关字段
    preview_texture: Option<egui::TextureHandle>, // 预览纹理
    preview_time: String,                         // 预览时间点
    preview_loading: bool,                        // 加载状态
    current_preview_frame: Arc<Mutex<Option<Vec<u8>>>>, // 共享预览帧数据
    last_preview_request_time: f64,               // 上次预览请求时间(用于防抖)
    preview_thread: Option<std::thread::JoinHandle<()>>, // 预览线程句柄
}

#[derive(Clone, Default)]
struct ProcessingState {
    progress: Arc<Mutex<f32>>,
    message: Arc<Mutex<String>>,
}

#[derive(Clone)]
struct BatchTask {
    input_path: String,
    output_path: String,
    start_time: String,
    end_time: String,
    rotation: i32,
}

impl VideoProcessor {
    fn load_config(&mut self) {
        let config_path = Path::new(&self.config_path);
        if config_path.exists() {
            if let Ok(config_str) = fs::read_to_string(config_path) {
                if let Ok(config) = serde_json::from_str::<AppConfig>(&config_str) {
                    self.output_dir = config.output_dir;
                }
            }
        }
    }

    fn save_config(&self) {
        let config = AppConfig {
            output_dir: self.output_dir.clone(),
        };
        if let Ok(config_str) = serde_json::to_string_pretty(&config) {
            let _ = fs::create_dir_all(Path::new(&self.config_path).parent().unwrap());
            let _ = fs::write(&self.config_path, config_str);
        }
    }
}

impl Default for VideoProcessor {
    fn default() -> Self {
        let config_path = format!("{}/.config/ffmpeg-gui.config", env!("HOME"));
        let mut processor = Self {
            source_paths: Vec::new(),
            output_dir: "output".to_string(),
            output_template: "{input_name}_processed_{rotation}_{timestamp}".to_string(),
            config_path,
            start_time: "0:00:00".to_owned(),
            end_time: "0:00:00".to_owned(),
            rotation: 0,
            batch_queue: Vec::new(),
            processing: false,
            state: ProcessingState::default(),
            preview_texture: None,
            preview_time: "0:00:00".to_owned(),
            preview_loading: false,
            current_preview_frame: Arc::new(Mutex::new(None)),
            last_preview_request_time: 0.0,
            preview_thread: None,
        };
        processor.load_config();
        processor
    }
}

impl eframe::App for VideoProcessor {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 处理文件拖放
        self.handle_file_drop(ctx);

        // 右侧预览面板
        egui::SidePanel::right("preview_panel")
            .resizable(true)
            .default_width(600.0)
            .show(ctx, |ui| {
                self.preview_panel(ui, ctx);
            });

        // 主内容区域
        egui::CentralPanel::default().show(ctx, |ui| {
            // 调整 间隙
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 30.0);
            ui.heading("视频处理工具");

            // 拖放提示
            ui.label("拖放文件到此区域或使用下方按钮添加文件");

            // 文件管理区域
            self.file_management_panel(ui);

            // 参数设置
            self.settings_panel(ui, ctx);

            // 处理控制
            self.process_control(ui);

            // 进度显示
            self.progress_display(ui);
        });
    }
}

// 图像加载辅助函数
fn load_image(data: &[u8]) -> Option<egui::ColorImage> {
    let image = image::load_from_memory(data).ok()?;
    let size = [image.width() as usize, image.height() as usize];
    let image_buffer = image.to_rgba8();
    let pixels = image_buffer.as_flat_samples();

    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        pixels.as_slice(),
    ))
}

impl VideoProcessor {
    // 预览生成方法
    fn generate_preview(&mut self, ctx: &egui::Context) {
        if self.source_paths.is_empty() || self.preview_loading {
            return;
        }

        // 防抖处理 - 至少间隔0.5秒才允许再次生成预览
        let now = ctx.input(|i| i.time);
        if now - self.last_preview_request_time < 0.5 {
            return;
        }
        self.last_preview_request_time = now;

        // 清理之前的预览线程
        if let Some(thread) = self.preview_thread.take() {
            thread.join().ok();
        }

        let input_path = self.source_paths[0].clone();
        let rotation = self.rotation;
        let time = self.preview_time.clone();
        let frame = self.current_preview_frame.clone();

        self.preview_loading = true;

        // 异步生成预览
        let ctx = ctx.clone();
        self.preview_thread = Some(std::thread::spawn(move || {
            let temp_path = "preview_temp.jpg";

            // 调用ffmpeg生成预览帧
            let mut args = vec!["-ss", &time, "-i", &input_path];

            // 仅当旋转角度非0时添加旋转滤镜
            let rotation_filter = format!("rotate={}*PI/180", rotation);
            if rotation != 0 {
                args.extend_from_slice(&["-vf", &rotation_filter]);
            }

            args.extend_from_slice(&["-vframes", "1", "-q:v", "2", "-y", temp_path]);

            // 修改后（添加状态检查）
            let status = Command::new("ffmpeg")
                .args(args)
                .status()
                .expect("Failed to execute ffmpeg");

            if !status.success() {
                eprintln!("Preview generation failed with code: {:?}", status.code());
            }

            // 读取生成的图片
            let img_data = std::fs::read(temp_path).ok();
            let _ = std::fs::remove_file(temp_path); // 清理临时文件

            // 更新到主线程
            let mut frame = frame.lock().unwrap();
            *frame = img_data;
            ctx.request_repaint();
        }));
    }

    // 在UI布局中增加预览面板
    fn preview_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label("预览时间点 (HH:MM:SS):");
            ui.text_edit_singleline(&mut self.preview_time);

            // 生成预览按钮
            if ui.button("🔄 生成预览").clicked() {
                self.generate_preview(ctx);
            }
        });

        // 显示加载状态
        if self.preview_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在生成预览...");
            });
        }

        // 显示预览图像
        if let Some(texture) = &self.preview_texture {
            let size = texture.size_vec2();
            let aspect_ratio = size.x / size.y;
            let max_width = 800.0;
            let max_height = 700.0;

            let (width, height) = if aspect_ratio > max_width / max_height {
                (max_width, max_width / aspect_ratio)
            } else {
                (max_height * aspect_ratio, max_height)
            };

            ui.image(texture, [width, height]);
        }
        // 异步更新纹理 - 只在有新帧数据时更新
        if let Ok(mut frame) = self.current_preview_frame.try_lock() {
            if let Some(img_data) = frame.take() {
                // 使用take()获取并清空数据
                if let Some(image) = load_image(&img_data) {
                    self.preview_texture =
                        Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR));
                    ctx.request_repaint(); // 主动请求重绘
                }
                self.preview_loading = false;
            }
        }
    }

    fn handle_file_drop(&mut self, ctx: &egui::Context) {
        let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());
        for file in &dropped_files {
            if let Some(path) = &file.path {
                let path_str = path.display().to_string();
                if !self.source_paths.contains(&path_str) {
                    self.source_paths.push(path_str);
                }
            }
        }
    }

    fn file_management_panel(&mut self, ui: &mut egui::Ui) {
        // 顶部固定区域
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label("已选文件:");
                    ui.label(format!("{} 个文件", self.source_paths.len()));
                });
                if ui.button("清空列表").clicked() {
                    self.source_paths.clear();
                }
            });
        });
        egui::ScrollArea::both()
            .max_height(100.0) // Fixed height scroll area
            .show(ui, |ui| {
                egui::Grid::new("file_grid").num_columns(3).show(ui, |ui| {
                    let mut paths_to_remove = Vec::new();
                    for path in &self.source_paths {
                        // ui.label(Path::new(path).file_name().unwrap().to_str().unwrap());
                        ui.label(path);
                        if ui.button("移除").clicked() {
                            paths_to_remove.push(path.clone());
                        }
                        ui.end_row();
                    }
                    self.source_paths.retain(|p| !paths_to_remove.contains(p));
                });
            });
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("参数设置");
        // 当start_time改变时，如果preview_time未被手动修改过，则同步更新preview_time
        let old_start_time = self.start_time.clone();
        let old_end_time = self.end_time.clone();
        let old_preview_time = self.preview_time.clone();
        let old_rotation = self.rotation.clone();

        // 输出目录
        ui.horizontal(|ui| {
            ui.label("输出目录:");
            ui.text_edit_singleline(&mut self.output_dir);
            if ui.button("选择...").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.output_dir = dir.display().to_string();
                    self.save_config();
                }
            }
        });

        // 文件名模板
        ui.horizontal(|ui| {
            ui.label("文件名模板:");
            ui.text_edit_singleline(&mut self.output_template);
            if ui.button("重置").clicked() {
                self.output_template = "{input_name}_processed_{rotation}_{timestamp}".to_string();
            }
        });
        ui.label("可用变量: {input_name} {rotation} {timestamp} {date} {time}");

        // 时间参数
        ui.horizontal(|ui| {
            ui.label("开始时间:");
            ui.text_edit_singleline(&mut self.start_time);
            ui.label("结束时间:");
            ui.text_edit_singleline(&mut self.end_time);
        });

        // 旋转参数
        ui.horizontal(|ui| {
            ui.label("旋转角度:");
            egui::ComboBox::from_id_source("rotation")
                .selected_text(format!("{}°", self.rotation))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.rotation, 0, "0°");
                    ui.selectable_value(&mut self.rotation, 90, "90°");
                    ui.selectable_value(&mut self.rotation, 180, "180°");
                    ui.selectable_value(&mut self.rotation, 270, "270°");
                });
        });

        // 如果start_time或end_time或rotation 被修改且preview_time未被手动修改过，则同步更新preview_time并生成预览
        if (self.start_time != old_start_time
            || self.end_time != old_end_time
            || self.rotation != old_rotation)
            && self.preview_time == old_preview_time
        {
            self.preview_time = self.start_time.clone();
            self.generate_preview(ctx);
        }
    }

    fn process_control(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("开始处理").clicked() && !self.processing {
                self.prepare_batch_tasks();
                self.processing = true;
                let state = self.state.clone();
                let tasks = self.batch_queue.clone();

                // 启动处理线程
                std::thread::spawn(move || {
                    for task in tasks {
                        *state.message.lock().unwrap() = format!("处理中: {}", task.input_path);
                        if let Err(e) = process_task(task, &state) {
                            *state.message.lock().unwrap() = format!("错误: {}", e);
                            break;
                        }
                    }
                    *state.message.lock().unwrap() = "处理完成".to_string();
                    *state.progress.lock().unwrap() = 0.0;
                });
            }

            if ui.button("停止").clicked() {
                self.processing = false;
            }
        });
    }

    fn progress_display(&self, ui: &mut egui::Ui) {
        let progress = *self.state.progress.lock().unwrap();
        ui.add(egui::ProgressBar::new(progress).text(format!("进度: {:.1}%", progress * 100.0)));

        let msg = self.state.message.lock().unwrap().clone();
        ui.label(msg);
    }

    fn prepare_batch_tasks(&mut self) {
        self.batch_queue = self
            .source_paths
            .iter()
            .map(|input_path| {
                let output_path = generate_output_path(
                    input_path,
                    &self.output_dir,
                    &self.output_template,
                    self.rotation,
                );
                BatchTask {
                    input_path: input_path.clone(),
                    output_path,
                    start_time: self.start_time.clone(), // 携带处理参数
                    end_time: self.end_time.clone(),
                    rotation: self.rotation,
                }
            })
            .collect();
    }
}

fn generate_output_path(
    input_path: &str,
    output_dir: &str,
    template: &str,
    rotation: i32,
) -> String {
    let now = Local::now();
    let input_path = Path::new(input_path);

    let replacements = [
        (
            "{input_name}",
            input_path.file_stem().unwrap().to_str().unwrap(),
        ),
        ("{rotation}", &rotation.to_string()),
        ("{timestamp}", &now.format("%Y%m%d%H%M%S").to_string()),
        ("{date}", &now.format("%Y-%m-%d").to_string()),
        ("{time}", &now.format("%H-%M-%S").to_string()),
    ];

    let mut filename = template.to_string();
    for (key, value) in &replacements {
        filename = filename.replace(key, value);
    }

    // 自动添加文件扩展名
    if let Some(ext) = input_path.extension() {
        if !filename.contains('.') {
            filename.push('.');
            filename.push_str(ext.to_str().unwrap());
        }
    }

    let output_path = Path::new(output_dir).join(filename);
    output_path.to_string_lossy().into_owned()
}

fn process_task(task: BatchTask, state: &ProcessingState) -> Result<(), String> {
    // 创建输出目录
    let output_path = Path::new(&task.output_path);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }

    // 构建基础命令
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // 添加输入文件
    cmd.arg("-i").arg(&task.input_path);

    // 添加时间裁剪参数
    if !task.start_time.is_empty() {
        cmd.arg("-ss").arg(&task.start_time);
    }
    if !task.end_time.is_empty() {
        cmd.arg("-to").arg(&task.end_time);
    }

    // 添加旋转元数据
    if task.rotation != 0 {
        cmd.args(&["-metadata:s:v", &task.rotation.to_string()]);
    }

    // 添加输出参数
    cmd.args(&["-c:v", "copy", "-c:a", "copy"])
        .arg(&task.output_path);

    // 启动子进程
    let mut child = cmd.spawn().map_err(|e| format!("启动FFmpeg失败: {}", e))?;

    // 获取stderr管道
    let stderr = child
        .stderr
        .take()
        .ok_or("无法获取stderr管道".to_string())?;

    // 启动进度监控线程
    let state_progress = state.progress.clone();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(line) = line {
                if let Some(progress) = parse_ffmpeg_progress(&line) {
                    *state_progress.lock().unwrap() = progress;
                }
            }
        }
    });

    // 等待处理完成
    let status = child
        .wait()
        .map_err(|e| format!("等待FFmpeg进程失败: {}", e))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("FFmpeg处理失败，退出码: {:?}", status.code()))
    }
}

fn parse_ffmpeg_progress(line: &str) -> Option<f32> {
    // 示例解析逻辑，实际需要根据FFmpeg输出调整
    if line.contains("time=") {
        let time_str = line.split("time=").nth(1)?.split(' ').next()?;
        let parts: Vec<&str> = time_str.split(':').collect();
        match parts.len() {
            3 => {
                // HH:MM:SS.ms
                let hours: f32 = parts[0].parse().ok()?;
                let minutes: f32 = parts[1].parse().ok()?;
                let seconds: f32 = parts[2].parse().ok()?;
                Some((hours * 3600.0 + minutes * 60.0 + seconds) / 100.0)
            }
            2 => {
                // MM:SS.ms
                let minutes: f32 = parts[0].parse().ok()?;
                let seconds: f32 = parts[1].parse().ok()?;
                Some((minutes * 60.0 + seconds) / 100.0)
            }
            _ => None,
        }
    } else {
        None
    }
}

fn setup_fonts(ctx: &egui::Context) {
    // 或者使用嵌入的字体文件（需将字体文件放在项目目录中）
    // let font_data = include_bytes!("../fonts/SourceHanSansSC-Regular.otf");

    let mut fonts = FontDefinitions::default();

    // 方式2：使用默认字体补充中文（推荐）
    fonts.font_data.insert(
        "my_font".to_owned(),
        egui::FontData::from_static(include_bytes!(
            //"/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"
            "../fonts/wqy-microhei.ttc"
        )),
    );

    // 设置主要字体
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "my_font".to_owned());

    // 设置等宽字体
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("my_font".to_owned());

    ctx.set_fonts(fonts);

    // 调整默认字体大小
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (
            egui::TextStyle::Heading,
            FontId::new(20.0, FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Body,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Monospace,
            FontId::new(14.0, FontFamily::Monospace),
        ),
    ]
    .into();
    ctx.set_style(style);
}
fn main() {
    // Load window icon
    let icon = {
        let icon_bytes = include_bytes!("../icons8-ffmpeg-48.png");
        let image = image::load_from_memory(icon_bytes).expect("Failed to load icon");
        let rgba = image.to_rgba8();
        eframe::IconData {
            rgba: rgba.to_vec(),
            width: image.width(),
            height: image.height(),
        }
    };

    let options = eframe::NativeOptions {
        initial_window_size: Some(egui::vec2(1200.0, 800.0)),
        // resizable: false,
        icon_data: Some(icon),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "视频处理工具",
        options,
        Box::new(|_cc| {
            setup_fonts(&_cc.egui_ctx);
            Box::new(VideoProcessor::default())
        }),
    );
}
