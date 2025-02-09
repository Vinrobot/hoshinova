use super::{Message, Module, Notification, Task, TaskStatus};
use crate::msgbus::BusTx;
use crate::{config::Config, module::RecordingStatus};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use lazy_static::lazy_static;
use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::{
    fs,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    sync::{mpsc, RwLock},
};
use ts_rs::TS;

use awscreds::Credentials;
use awsregion::Region;
use s3::Bucket;

pub struct YTArchive {
    config: Arc<RwLock<Config>>,
    active_ids: Arc<RwLock<HashSet<String>>>,
}

impl YTArchive {
    async fn record(cfg: Config, task: Task, bus: &mut BusTx<Message>) -> Result<()> {
        let task_name = format!("[{}][{}][{}]", task.video_id, task.channel_name, task.title);

        let bucket: Option<Bucket>;
        if let Some(storage) = cfg.storage {
            let s3 = storage.s3;
            let credentials = Credentials::new(
                s3.access_key.as_deref(),
                s3.secret_key.as_deref(),
                None,
                None,
                None,
            )?;

            let region: Region;
            if let Some(endpoint) = s3.endpoint.as_ref() {
                let region_name = match s3.region {
                    Some(v) => v,
                    None => endpoint.to_string(),
                };
                region = Region::Custom { region: region_name, endpoint: endpoint.to_string() };
            } else if let Some(region_name) = s3.region {
                region = region_name.parse()?;
            } else {
                region = Region::UsEast1;
            }

            if s3.path_style.unwrap_or(false) {
                bucket = Some(Bucket::new(&*s3.bucket, region, credentials)?.with_path_style());
            } else {
                bucket = Some(Bucket::new(&*s3.bucket, region, credentials)?);
            }
        } else {
            bucket = None;
        }

        // Ensure the working directory exists
        let cfg = cfg.ytarchive;
        tokio::fs::create_dir_all(&cfg.working_directory)
            .await
            .map_err(|e| anyhow!("Failed to create working directory: {}", e))?;

        // Ensure the output directory exists
        tokio::fs::create_dir_all(&task.output_directory)
            .await
            .map_err(|e| anyhow!("Failed to create output directory: {:?}", e))?;

        // Construct the command line arguments
        let mut args = cfg.args.clone();

        // Add the --wait flag if not present
        if !args.contains(&"-w".to_string()) && !args.contains(&"--wait".to_string()) {
            args.push("--wait".to_string());
        }

        args.extend(vec![
            format!("https://youtu.be/{}", task.video_id),
            cfg.quality.clone(),
        ]);

        // Start the process
        debug!("{} Starting ytarchive with args {:?}", task_name, args);
        let mut process = tokio::process::Command::new(&cfg.executable_path)
            .args(args)
            .current_dir(&cfg.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to start process: {}", e))?;

        // Grab stdout/stderr byte iterators
        let mut stdout = BufReader::new(
            process
                .stdout
                .take()
                .ok_or(anyhow!("Failed to take stdout"))?,
        );
        let mut stderr = BufReader::new(
            process
                .stderr
                .take()
                .ok_or(anyhow!("Failed to take stderr"))?,
        );

        // Create a channel to consolidate stdout and stderr
        let (tx, mut rx) = mpsc::channel(1);

        // Flag to mark when the process has exited
        let done = Arc::from(AtomicBool::new(false));

        macro_rules! read_line {
            ($reader:expr, $tx:expr) => {{
                // Read bytes until a \r or \n is returned
                let mut bytes = Vec::new();
                loop {
                    match $reader.read_u8().await {
                        Ok(byte) => {
                            if byte == b'\r' || byte == b'\n' {
                                break;
                            }
                            bytes.push(byte);
                        }
                        _ => break,
                    }
                }

                // Skip if there are no bytes
                if bytes.is_empty() {
                    continue;
                }

                // Convert to a string
                let line = match std::str::from_utf8(&bytes) {
                    Ok(line) => line.to_owned(),
                    Err(e) => {
                        trace!("Failed to read utf8: {:?}", e);
                        break;
                    }
                };

                // Send the line to the channel
                if let Err(e) = $tx.send(line).await {
                    trace!("Failed to send line: {:?}", e);
                    break;
                }
            }};
        }

        // Read stdout
        let done_clone = done.clone();
        let task_name_clone = task_name.clone();
        let tx_clone = tx.clone();
        let h_stdout = tokio::spawn(async move {
            while !done_clone.load(Ordering::Relaxed) {
                read_line!(&mut stdout, tx_clone);
            }
            trace!("{} stdout reader exited", task_name_clone);
        });

        // Read stderr
        let done_clone = done.clone();
        let task_name_clone = task_name.clone();
        let tx_clone = tx.clone();
        let h_stderr = tokio::spawn(async move {
            while !done_clone.load(Ordering::Relaxed) {
                read_line!(&mut stderr, tx_clone);
            }
            trace!("{} stderr reader exited", task_name_clone);
        });

        // Wait for the process to exit
        let done_clone = done.clone();
        let task_name_clone = task_name.clone();
        let h_wait = tokio::spawn(async move {
            let result = process.wait().await;

            // Wait a bit for the stdout to be completely read
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Stop threads
            done_clone.store(true, Ordering::Relaxed);
            debug!("{} Process exited with {:?}", task_name_clone, result);

            // Send a blank message to unblock the status monitor thread
            let _ = tx.send("".into());

            result
        });

        // Parse each line
        let mut status = YTAStatus::new();
        loop {
            let line = match rx.recv().await {
                Some(line) => line,
                None => break,
            };

            // Stop when done
            if done.load(Ordering::Relaxed) {
                break;
            }

            trace!("{}[yta:out] {}", task_name, line);

            let old = status.clone();
            status.parse_line(&line);

            // Push the current status to the bus
            if let Err(_) = bus
                .send(Message::RecordingStatus(RecordingStatus {
                    task: task.clone(),
                    status: status.clone(),
                }))
                .await
            {
                break;
            }

            // Check if status changed
            if old.state == status.state {
                continue;
            }

            let message = match status.state {
                YTAState::Waiting(_) => {
                    info!("{} Waiting for stream to go live", task_name);
                    Some(Message::ToNotify(Notification {
                        task: task.clone(),
                        status: TaskStatus::Waiting,
                    }))
                }
                YTAState::Recording => {
                    info!("{} Recording started", task_name);
                    Some(Message::ToNotify(Notification {
                        task: task.clone(),
                        status: TaskStatus::Recording,
                    }))
                }
                YTAState::Finished => {
                    info!("{} Recording finished", task_name);
                    Some(Message::ToNotify(Notification {
                        task: task.clone(),
                        status: TaskStatus::Done,
                    }))
                }
                YTAState::AlreadyProcessed => {
                    info!("{} Video already processed, skipping", task_name);
                    None
                }
                YTAState::Interrupted => {
                    info!("{} Recording failed: interrupted", task_name);
                    Some(Message::ToNotify(Notification {
                        task: task.clone(),
                        status: TaskStatus::Failed,
                    }))
                }
                _ => None,
            };

            if let Some(message) = message {
                // Exit the loop if message failed to send
                if let Err(_) = bus.send(message).await {
                    break;
                }
            }
        }

        trace!("{} Status loop exited: {:?}", task_name, status);

        // Wait for threads to finish
        let (r_wait, r_stdout, r_stderr) = futures::join!(h_wait, h_stdout, h_stderr);
        trace!("{} Process monitor exited: {:?}", task_name, r_wait);
        trace!("{} Stdout monitor quit: {:?}", task_name, r_stdout);
        trace!("{} Stderr monitor quit: {:?}", task_name, r_stderr);

        // Skip moving files if it didn't finish
        if status.state != YTAState::Finished {
            return Ok(());
        }

        // Move the video to the output directory
        let frompath = status
            .output_file
            .ok_or(anyhow!("ytarchive did not emit an output file"))?;
        let frompath = Path::new(&frompath);
        let filename = frompath
            .file_name()
            .ok_or(anyhow!("Failed to get filename"))?;
        let destpath = Path::new(&task.output_directory).join(filename);

        if let Some(bucket) = bucket {
            info!("{} Uploading to storage", task_name);

            let fname = destpath.as_path().display().to_string();
            let mut path = tokio::fs::File::open(frompath).await?;
            let status_code = bucket.put_object_stream(&mut path, &*fname).await?;
            if status_code != 200 {
                return Err(anyhow!("Failed to upload record to storage (status code: {})", status_code));
            }

            info!(
                "{} Uploaded output file to s3://{}, removing original",
                task_name,
                destpath.display(),
            );
            fs::remove_file(frompath)
                .map_err(|e| anyhow!("Failed to remove original file: {:?}", e))?;
        } else if let Err(_) = fs::rename(frompath, &destpath) {
            debug!(
                "{} Failed to rename file to output, trying to copy",
                task_name,
            );

            // Copy the file into the output directory
            fs::copy(frompath, &destpath)
                .map_err(|e| anyhow!("Failed to copy file to output: {:?}", e))?;
            info!(
                "{} Copied output file to {}, removing original",
                task_name,
                destpath.display(),
            );
            fs::remove_file(frompath)
                .map_err(|e| anyhow!("Failed to remove original file: {:?}", e))?;
        }

        info!("{} Moved output file to {}", task_name, destpath.display());
        Ok(())
    }
}

#[async_trait]
impl Module for YTArchive {
    fn new(config: Arc<RwLock<Config>>) -> Self {
        let active_ids = Arc::new(RwLock::new(HashSet::new()));
        Self { config, active_ids }
    }

    async fn run(&self, tx: &BusTx<Message>, rx: &mut mpsc::Receiver<Message>) -> Result<()> {
        // Listen for new messages
        while let Some(message) = rx.recv().await {
            match message {
                Message::ToRecord(task) => {
                    // Check if the task is already active
                    if self.active_ids.read().await.contains(&task.video_id) {
                        warn!("Task {} is already active, skipping", task.video_id);
                        continue;
                    }

                    debug!("Spawning thread for task: {:?}", task);
                    let mut tx = tx.clone();
                    let cfg = self.config.read().await;
                    let cfg = cfg.clone();
                    let active_ids = self.active_ids.clone();
                    tokio::spawn(async move {
                        let video_id = task.video_id.clone();
                        active_ids.write().await.insert(video_id.clone());

                        if let Err(e) = YTArchive::record(cfg, task, &mut tx).await {
                            error!("Failed to record task: {:?}", e);
                        };

                        active_ids.write().await.remove(&video_id);
                    });

                    // Wait a bit before starting the next task
                    let delay = self.config.read().await.ytarchive.delay_start;
                    tokio::time::sleep(delay).await;
                }
                _ => (),
            }
        }

        debug!("YTArchive module finished");
        Ok(())
    }
}

/// The current state of ytarchive.
#[derive(Debug, Clone, TS, Serialize)]
#[ts(export, export_to = "web/src/bindings/")]
pub struct YTAStatus {
    version: Option<String>,
    state: YTAState,
    last_output: Option<String>,
    last_update: chrono::DateTime<chrono::Utc>,
    video_fragments: Option<u32>,
    audio_fragments: Option<u32>,
    total_size: Option<String>,
    video_quality: Option<String>,
    output_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, TS, Serialize)]
#[ts(export, export_to = "web/src/bindings/")]
pub enum YTAState {
    Idle,
    Waiting(Option<DateTime<Utc>>),
    Recording,
    Muxing,
    Finished,
    AlreadyProcessed,
    Ended,
    Interrupted,
}

fn strip_ansi(s: &str) -> String {
    lazy_static! {
        static ref RE: Regex = Regex::new(concat!(
            r"[\u001B\u009B][[\\]()#;?]*",
            r"(?:(?:(?:[a-zA-Z\\d]*(?:;[a-zA-Z\\d]*)*)?\u0007)|",
            r"(?:(?:\\d{1,4}(?:;\\d{0,4})*)?[\\dA-PRZcf-ntqry=><~]))",
        ))
        .expect("Failed to compile ANSI stripping regex");
    }
    let stripped = RE.replace_all(s, "").to_string();
    stripped
        .strip_suffix("\u{001b}[K")
        .unwrap_or(&stripped)
        .to_string()
}

impl YTAStatus {
    pub fn new() -> Self {
        Self {
            version: None,
            state: YTAState::Idle,
            last_output: None,
            last_update: chrono::Utc::now(),
            video_fragments: None,
            audio_fragments: None,
            total_size: None,
            video_quality: None,
            output_file: None,
        }
    }

    /// parse_line parses a line of output from the ytarchive process.
    ///
    /// Sample output:
    ///
    ///   ytarchive 0.3.1-15663af
    ///   Stream starts at 2022-03-14T14:00:00+00:00 in 11075 seconds. Waiting for this time to elapse...
    ///   Stream is 30 seconds late...
    ///   Selected quality: 1080p60 (h264)
    ///   Video Fragments: 1215; Audio Fragments: 1215; Total Downloaded: 133.12MiB
    ///   Download Finished
    ///   Muxing final file...
    ///   Final file: /path/to/output.mp4
    pub fn parse_line(&mut self, line: &str) {
        self.last_output = Some(line.to_string());
        self.last_update = chrono::Utc::now();

        if line.starts_with("Video Fragments: ") {
            self.state = YTAState::Recording;
            let mut parts = line.split(';').map(|s| s.split(':').nth(1).unwrap_or(""));
            if let Some(x) = parts.next() {
                self.video_fragments = x.trim().parse().ok();
            };
            if let Some(x) = parts.next() {
                self.audio_fragments = x.trim().parse().ok();
            };
            if let Some(x) = parts.next() {
                self.total_size = Some(strip_ansi(x.trim()));
            };
        } else if self.version == None && line.starts_with("ytarchive ") {
            self.version = Some(strip_ansi(&line[10..]));
        } else if self.video_quality == None && line.starts_with("Selected quality: ") {
            self.video_quality = Some(strip_ansi(&line[18..]));
        } else if line.starts_with("Stream starts at ") {
            let date = DateTime::parse_from_rfc3339(&line[17..42])
                .ok()
                .map(|d| d.into());
            self.state = YTAState::Waiting(date);
        } else if line.starts_with("Stream is ") || line.starts_with("Waiting for stream") {
            self.state = YTAState::Waiting(None);
        } else if line.starts_with("Muxing final file") {
            self.state = YTAState::Muxing;
        } else if line.starts_with("Livestream has been processed") {
            self.state = YTAState::AlreadyProcessed;
        } else if line.starts_with("Livestream has ended and is being processed")
            || line.contains("use yt-dlp to download it.")
        {
            self.state = YTAState::Ended;
        } else if line.starts_with("Final file: ") {
            self.state = YTAState::Finished;
            self.output_file = Some(strip_ansi(&line[12..]));
        } else if line.contains("User Interrupt") {
            self.state = YTAState::Interrupted;
        } else if line.trim().is_empty() || line.contains("Loaded cookie file") {
            // Ignore
        } else {
            warn!("Unknown ytarchive output: {}", line);
        }
    }
}
