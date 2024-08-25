use std::{path::PathBuf, fs, process::{Command, ExitStatus}, path::Path, env};
use anyhow::{Result, anyhow};
use dirs::{config_dir, home_dir};
use clap::{Parser, crate_name};
use serde::{Deserialize, Serialize};
use blake3::Hasher;

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Could not find config directory")]
    ConfigDirNotFound,
    #[error("Invalid video path: {0}")]
    InvalidVideoPath(PathBuf),
    #[error("Failed to execute ab-av1 command: {0}")]
    AbAv1CommandFailed(ExitStatus),
    #[error("Conflict between video path and encoding video path {1:?} for video {0:?}")]
    ConflictVideoEncoding(PathBuf, PathBuf),
}

#[derive(Deserialize, Serialize, Debug)]
struct Config {
    save_dir: PathBuf,
    tmp_dir: PathBuf,
    min_crf: u8,
    max_crf: u8,
    max_encoded_percent: u8,
}

impl Default for Config {
    fn default() -> Self {
        let home_dir = home_dir().expect("home directory must exist");
        Self {
            save_dir: home_dir.join("Videos").join("av1_encoded"),
            tmp_dir: home_dir.join("Videos").join("av1_tmp"),
            min_crf: 15,
            max_crf: 50,
            max_encoded_percent: 70,
        }
    }
}

#[derive(Parser, Debug)]
struct Args {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Parser, Debug)]
enum SubCommand {
    All(AllOpts),
    DebugSingle(DebugSingleOpts),
}

#[derive(Parser, Debug)]
struct AllOpts {
    video_dir: PathBuf,
    target_vmaf: u8,
}

#[derive(Parser, Debug)]
struct DebugSingleOpts {
    video_path: PathBuf,
    target_vmaf: u8,
}

fn main() -> Result<()> {
    env_logger::init();
    let config = prepare_config()?;
    log::info!("Config: {:?}", config);

    let args = Args::parse();
    match args.subcmd {
        SubCommand::All(opts) => run_all(opts, config)?,
        SubCommand::DebugSingle(opts) => run_debug_single_command(opts, config)?,
    }

    Ok(())
}

fn run_all(opts: AllOpts, config: Config) -> Result<()> {
    let video_paths = walk_dir(&opts.video_dir)?;
    let encodnig_video_dir = config.tmp_dir.join("encoding");
    let save_dir = &config.save_dir;

    let inherited_log_level = env::var("RUST_LOG").unwrap_or("warn".to_string());
    log::debug!("Inherited log level: {}", inherited_log_level);

    for video_path in video_paths {
        fs::create_dir_all(&encodnig_video_dir)?;
        fs::create_dir_all(&save_dir)?;

        // file_stem sometimes treats the last part of the file name as extension
        // so we impl the way below
        let video_filename = video_path.file_name().ok_or(Error::InvalidVideoPath(video_path.clone()))?;
        let mut iter = video_filename.as_encoded_bytes().rsplitn(1, |&b| b == b'.');
        let video_slug = iter.next().ok_or(Error::InvalidVideoPath(video_path.clone()))?;
        let video_slug = String::from_utf8_lossy(video_slug).to_string();
        log::debug!("Video slug: {}", video_slug);

        let video_location_hash = hash_file_location(&video_path);
        let encoding_video_path = encodnig_video_dir.join(&video_location_hash).with_extension("mkv");
        let save_path = save_dir.join(&video_slug).with_extension("mkv");

        if save_path.exists() {
            log::info!("Skipping video {:?} as it already exists in save directory", video_path);
            continue;
        }

        if encoding_video_path.exists() {
            return Err(anyhow!(Error::ConflictVideoEncoding(video_path, encoding_video_path)));
        }

        println!("Encoding video: {}", video_filename.to_string_lossy());
        match exec_ab_av1(&video_path, &encoding_video_path, opts.target_vmaf, false, &inherited_log_level, &config) {
            Ok(_) => log::info!("Encoding successful for {:?}", video_path),
            Err(e) => {
                if encoding_video_path.exists() {
                    fs::remove_file(&encoding_video_path)?;
                }
                match e.downcast_ref::<Error>() {
                    Some(Error::AbAv1CommandFailed(_)) => {
                        log::warn!("Encoding failed for {:?}: {:?}", video_path, e);
                        continue;
                    },
                    _ => return Err(e),
                }
            }
        }

        if encoding_video_path.exists() && !is_valid_video_file(&encoding_video_path)? {
            log::warn!("Encoding failed for {:?}: Invalid video file", video_path);
            fs::remove_file(&encoding_video_path)?;
            continue;
        }

        println!("Saving video: {}", video_filename.to_string_lossy());
        match fs::rename(&encoding_video_path, &save_path) {
            Ok(_) => log::info!("Saved encoded video to {:?}", save_path),
            Err(e) => {
                match e.raw_os_error() {
                    Some(libc::EXDEV) => {
                        fs::copy(&encoding_video_path, &save_path)?;
                        fs::remove_file(&encoding_video_path)?;
                        log::info!("Saved encoded video (over device) to {:?}", save_path);
                    },
                    _ => return Err(e.into()),
                }
            }
        }
    }

    Ok(())
}

fn run_debug_single_command(opts: DebugSingleOpts, config: Config) -> Result<()> {
    let output_path = config.save_dir.join("output.mp4");

    log::info!("Running debug single command with opts: {:?}", opts);
    log::info!("Output path: {:?}", output_path);

    exec_ab_av1(&opts.video_path, &output_path, opts.target_vmaf, true, "debug", &config)
}

fn exec_ab_av1(input_path: impl AsRef<Path>, output_path: impl AsRef<Path>, target_vmaf: u8, debug_intermediate_files: bool, log_level: impl AsRef<str>, config: &Config) -> Result<()> {
    let input_path = input_path.as_ref();
    let output_path = output_path.as_ref();
    let log_level = log_level.as_ref();
    let tmp_dir = if debug_intermediate_files {
        PathBuf::from(".")
    } else {
        config.tmp_dir.join("ab_av1_tmp")
    };
    fs::create_dir_all(&tmp_dir)?;
    let mut command = Command::new("ab-av1");
    command
        .env("RUST_BACKTRACE", "1")
        .env("RUST_LOG", format!("ab_av1={}", log_level))
        .arg("auto-encode")
        .arg("-e").arg("av1_nvenc")
        .arg("--cuda")
        .arg("--enc").arg("v:b=0").arg("--enc").arg("rc=vbr")
        .arg("--enc").arg("temporal-aq=1")
        .arg("--enc").arg("tune=hq")
        .arg("--enc").arg("rc-lookahead=32")
        .arg("--enc").arg("fps_mode=passthrough")
        .arg("--enc").arg("sn").arg("--enc").arg("dn").arg("--acodec").arg("aac")
        .arg("--preset").arg("p7")
        .arg("--min-vmaf").arg(target_vmaf.to_string())
        .arg("--min-crf").arg(config.min_crf.to_string())
        .arg("--max-crf").arg(config.max_crf.to_string())
        .arg("--max-encoded-percent").arg(config.max_encoded_percent.to_string())
        .arg("--temp-dir").arg(tmp_dir)
        .arg("-i").arg(input_path)
        .arg("-o").arg(output_path);

    if debug_intermediate_files {
        command.arg("--keep");
    }
    log::debug!("Command: {:?}", command);
    let status = command.status()?;
    log::debug!("Command status: {:?}", status);
    if status.success() {
       Ok(())
    } else {
        Err(anyhow!(Error::AbAv1CommandFailed(status)))
    }
}

fn prepare_config() -> Result<Config> {
    let config_parent_dir = config_dir().ok_or(Error::ConfigDirNotFound)?;
    let config_dir = config_parent_dir.join(crate_name!());
    fs::create_dir_all(&config_dir)?;

    let config_path = config_dir.join("config.toml");
    if !config_path.exists() {
        let default_config = Config::default();
        let toml = toml::to_string_pretty(&default_config)?;
        std::fs::write(&config_path, toml)?;
        log::info!("Default config written to {:?}", config_path);
    }
    let config = config::Config::builder()
        .add_source(config::File::from(config_path))
        .build()?;
    let config = config.try_deserialize::<Config>()?;

    Ok(config)
}

fn is_valid_video_file(video_path: impl AsRef<Path>) -> Result<bool> {
    let video_path = video_path.as_ref();

    let mut command = Command::new("ffprobe");
    command
        .arg("-v").arg("error")
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("stream=width,height")
        .arg("-of").arg("csv=p=0")
        .arg(video_path);
    log::debug!("Command: {:?}", command);
    let status = command.status()?;
    log::debug!("Command status: {:?}", status);

    Ok(status.success())
}

fn walk_dir(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            paths.extend(walk_dir(&path)?);
        } else {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn hash_file_location(file_path: impl AsRef<Path>) -> String {
    let file_path = file_path.as_ref();
    let file_path_bytes = file_path.as_os_str().as_encoded_bytes();

    let mut hasher = Hasher::new();
    hasher.update(file_path_bytes);
    let hash = hasher.finalize();
    hash.to_hex().to_string()
}

