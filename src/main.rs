use std::{path::PathBuf, fs, process::{Command, ExitStatus}, path::Path, env};
use anyhow::{Result, anyhow};
use dirs::home_dir;
use clap::{Parser, crate_name};
use serde::{Deserialize, Serialize};
use blake3::Hasher;
use junk_file::is_junk;

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Invalid video path: {0}")]
    InvalidVideoPath(PathBuf),
    #[error("Failed to execute ab-av1 command: {0}")]
    AbAv1CommandFailed(ExitStatus),
    #[error("Failed to execute force crf ffmpeg command: {0}")]
    ForceCrfFfmpegCommandFailed(ExitStatus),
    #[error("Conflict encoding video path {1:?} for video {0:?}")]
    ConflictVideoEncoding(PathBuf, PathBuf),
    #[error("Conflict failed copy path {1:?} for video {0:?}")]
    ConflictFailedCopyPath(PathBuf, PathBuf),
    #[error("Save path already exists for single encode: {0}")]
    SingleEncodeSavePathAlreadyExists(PathBuf),
    #[error("Single encode failed with invalid encoded file: {0}")]
    SingleEncodeFailedWithInvalidEncodedFile(PathBuf, PathBuf),
    #[error("Failed to execute ffprobe check valid video: {0}")]
    FfprobeCheckValidVideoFailed(String),
    #[error("Failed to execute ffprobe show duration: {0}")]
    FfprobeShowDurationFailed(String),
    #[error("Failed to parse duration decounds string: {0}")]
    ParseDurationSecondsFailed(String),
    #[error("Found invalid video file in saved path: {0}")]
    FoundInvalidVideoFileInSavedPath(PathBuf),
    #[error("Renamer command failed: {0}")]
    RenamerCommandFailed(String, ExitStatus),
    #[error("Too many characters in renamed filename: {0}")]
    TooManyCharsInRenamedFilename(String),
    #[error("Too many bytes in renamed filename: {0}")]
    TooManyBytesInRenamedFilename(String),
}

#[derive(Deserialize, Serialize, Debug)]
struct Config {
    save_dir: PathBuf,
    tmp_dir: PathBuf,
    min_crf: u8,
    max_crf: u8,
    max_encoded_percent: u8,
    keep_original: bool,
    move_failed_files: bool,
    delete_almost_same_files: bool,
    #[serde(default)]
    renamer: Option<RenamerConfig>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RenamerConfig {
    command: String,
    args: Vec<String>,
    bytes_limit: usize,
    chars_limit: usize,
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
            keep_original: true,
            move_failed_files: false,
            delete_almost_same_files: false,
            renamer: None,
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
    ForceCrfSingle(ForceCrfSingleOpts),
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

#[derive(Parser, Debug)]
struct ForceCrfSingleOpts {
    video_path: PathBuf,
    crf: u8,
}

fn main() -> Result<()> {
    env_logger::init();
    jdt::use_from(crate_name!());
    let config = jdt::config();
    log::debug!("Config: {:?}", config);

    let args = Args::parse();
    match args.subcmd {
        SubCommand::All(opts) => run_all(opts, config)?,
        SubCommand::DebugSingle(opts) => run_debug_single_command(opts, config)?,
        SubCommand::ForceCrfSingle(opts) => run_force_crf_single_command(opts, config)?,
    }

    Ok(())
}

fn run_all(opts: AllOpts, config: Config) -> Result<()> {
    let video_paths = jdt::walk_dir(&opts.video_dir, |path| path);
    let encodnig_video_dir = config.tmp_dir.join("encoding");
    let save_dir = &config.save_dir;

    let inherited_log_level = env::var("RUST_LOG").unwrap_or("warn".to_string());
    log::debug!("Inherited log level: {}", inherited_log_level);

    let move_failed_files = config.move_failed_files;
    let delete_almost_same_files = config.delete_almost_same_files;

    for video_path in video_paths {
        log::trace!("Iterate path: {}", video_path.display());

        fs::create_dir_all(&encodnig_video_dir)?;
        fs::create_dir_all(&save_dir)?;

        // file_stem sometimes treats the last part of the file name as extension
        // so we impl the way below
        let video_location_hash = hash_file_location(&video_path);
        let encoding_video_path = encodnig_video_dir.join(&video_location_hash).with_extension("mkv");
        let save_path = encoded_file_save_path(&video_path, &config)?;

        let dst_video_filename = destination_filename(&video_path, &config)?;
        let failed_copy_path = save_dir.join(dst_video_filename);

        if is_junk(&video_path) {
            println!("Removing junk file: {}", video_path.display());
            fs::remove_file(&video_path)?;
            continue;
        }

        if !guess_video_file(&video_path) {
            println!("Skipping non-video file: {}", video_path.display());
            continue;
        }

        if !is_valid_video_file(&video_path)? {
            println!("Skipping invalid video file: {}", video_path.display());
            continue;
        }

        if save_path.exists() {
            if delete_almost_same_files {
                if !is_valid_video_file(&save_path)? {
                    return Err(anyhow!(Error::FoundInvalidVideoFileInSavedPath(save_path)));
                }

                let duration_of_saved_video = rough_video_secs(&save_path)?;
                let duration_of_current_video = rough_video_secs(&video_path)?;

                if jdt::almost_eq(duration_of_saved_video, duration_of_current_video, 0.01) {
                    println!("Removing a file having duplicate name, almost equal duration video: {}", video_path.display());
                    fs::remove_file(&video_path)?;
                } else {
                    println!("Skipping video for now, duplicated names, but different durations ({} != {}): {}", duration_of_saved_video, duration_of_current_video, save_path.display());
                }
            } else {
                println!("Skipping video {} as it already exists in save directory", video_path.display());
            }
            continue;
        }

        if move_failed_files && failed_copy_path.exists() {
            return Err(anyhow!(Error::ConflictFailedCopyPath(video_path, failed_copy_path)));
        }

        if encoding_video_path.exists() {
            return Err(anyhow!(Error::ConflictVideoEncoding(video_path, encoding_video_path)));
        }

        println!("Encoding video: {}", video_path.display());
        let success = match exec_ab_av1(&video_path, &encoding_video_path, opts.target_vmaf, false, &inherited_log_level, &config) {
            Ok(_) => true,
            Err(e) => {
                match e.downcast_ref::<Error>() {
                    Some(Error::AbAv1CommandFailed(_)) => false,
                    _ => return Err(e),
                }
            }
        };

        if success {
            if encoding_video_path.exists() && !is_valid_video_file(&encoding_video_path)? {
                log::warn!("Encoding failed for {:?}: Invalid video file", video_path);
                fs::remove_file(&encoding_video_path)?;
                continue;
            }

            let start_saving = std::time::Instant::now();
            println!("Saving video to: {}", save_path.display());
            jdt::rename_file(&encoding_video_path, &save_path)?;
            let elapsed = start_saving.elapsed();
            if elapsed.as_secs() > 10 {
                println!("Saved in {:.2} sec", elapsed.as_secs_f64());
            }

            if !config.keep_original {
                println!("Removing original video ...");
                fs::remove_file(&video_path)?;
                log::debug!("Removed original video {:?}", video_path);
            }
        } else {
            if encoding_video_path.exists() {
                fs::remove_file(&encoding_video_path)?;
            }
            if move_failed_files {
                println!("Moving failed video ...");
                jdt::rename_file(&video_path, &failed_copy_path)?;
            }
        }
    }

    Ok(())
}

fn run_debug_single_command(opts: DebugSingleOpts, config: Config) -> Result<()> {
    let output_path = config.save_dir.join("output.mp4");

    log::debug!("Running debug single command with opts: {:?}", opts);
    log::debug!("Output path: {:?}", output_path);

    exec_ab_av1(&opts.video_path, &output_path, opts.target_vmaf, true, "debug", &config)
}

fn run_force_crf_single_command(opts: ForceCrfSingleOpts, config: Config) -> Result<()> {
    let save_dir = &config.save_dir;
    let encodnig_video_dir = config.tmp_dir.join("encoding");
    let video_path = &opts.video_path;
    fs::create_dir_all(&save_dir)?;
    fs::create_dir_all(&encodnig_video_dir)?;

    let video_location_hash = hash_file_location(&video_path);
    let encoding_video_path = encodnig_video_dir.join(&video_location_hash).with_extension("mkv");
    let save_path = encoded_file_save_path(&opts.video_path, &config)?;

    if save_path.exists() {
        return Err(anyhow!(Error::SingleEncodeSavePathAlreadyExists(save_path)));
    }

    if encoding_video_path.exists() {
        return Err(anyhow!(Error::ConflictVideoEncoding(video_path.clone(), encoding_video_path.clone())));
    }

    println!("Encoding video: {}", video_path.display());
    exec_force_crf_ffmpeg(&opts.video_path, &encoding_video_path, opts.crf)?;

    if encoding_video_path.exists() && !is_valid_video_file(&encoding_video_path)? {
        fs::remove_file(&encoding_video_path)?;
        return Err(anyhow!(Error::SingleEncodeFailedWithInvalidEncodedFile(video_path.clone(), encoding_video_path.clone())));
    }

    let start_saving = std::time::Instant::now();
    println!("Saving video to: {}", save_path.display());
    jdt::rename_file(&encoding_video_path, &save_path)?;
    let elapsed = start_saving.elapsed();
    if elapsed.as_secs() > 10 {
        println!("Saved in {:.2} sec", elapsed.as_secs_f64());
    }

    if !config.keep_original {
        println!("Removing original video ...");
        fs::remove_file(&video_path)?;
        log::debug!("Removed original video {:?}", video_path);
    }

    Ok(())
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

// VMAF sometimes gives wrong results than human-sense score, for example, the reference video with VHD frame-vibrations, etc.
// So, we support the feature just to set constant quality for ffmpeg
fn exec_force_crf_ffmpeg(input_path: impl AsRef<Path>, output_path: impl AsRef<Path>, crf: u8) -> Result<()> {
    let input_path = input_path.as_ref();
    let output_path = output_path.as_ref();
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hwaccel").arg("cuda").arg("-hwaccel_output_format").arg("cuda")
        .arg("-i").arg(input_path)
        .arg("-c:v").arg("av1_nvenc")
        .arg("-v:b").arg("0").arg("-rc").arg("vbr")
        .arg("-preset").arg("p7")
        .arg("-fps_mode").arg("passthrough")
        .arg("-tune").arg("hq")
        .arg("-temporal-aq").arg("1")
        .arg("-rc-lookahead").arg("32")
        .arg("-g").arg("300")
        .arg("-cq").arg(crf.to_string())
        .arg("-highbitdepth").arg("1")
        .arg("-sn").arg("-dn")
        .arg("-acodec").arg("aac")
        .arg(output_path);

    log::debug!("Command: {:?}", command);
    let status = command.status()?;
    log::debug!("Command status: {:?}", status);
    if status.success() {
       Ok(())
    } else {
        Err(anyhow!(Error::ForceCrfFfmpegCommandFailed(status)))
    }
}

fn encoded_file_save_path(video_path: impl AsRef<Path>, config: &Config) -> Result<PathBuf> {
    let video_path = video_path.as_ref();
    let save_dir = &config.save_dir;

    // file_stem sometimes treats the last part of the file name as extension
    // so we impl the way below
    let video_filename = video_path.file_name().ok_or(Error::InvalidVideoPath(video_path.to_path_buf()))?;
    let mut iter = video_filename.as_encoded_bytes().rsplitn(1, |&b| b == b'.');
    let video_slug = iter.next().ok_or(Error::InvalidVideoPath(video_path.to_path_buf()))?;
    let video_slug = String::from_utf8_lossy(video_slug).to_string();

    let pre_save_path = save_dir.join(&video_slug).with_extension("mkv");
    let save_video_filename = destination_filename(&pre_save_path, config)?;
    let save_path = save_dir.join(save_video_filename);

    Ok(save_path)
}

fn guess_video_file(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    let guess = mime_guess::from_path(path);
    let mut iter = guess.iter();
    iter.any(|mime| mime.type_() == "video")
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
    let output = command.output().map_err(|e| Error::FfprobeCheckValidVideoFailed(format!("{:?}", e)))?;
    log::debug!("Command status: {:?}", output.status);

    if !output.status.success() {
        return Ok(false);
    }

    // check w,h
    let stdout_str = output.stdout;
    let stdout_str = String::from_utf8_lossy(&stdout_str);

    // let take the first line
    let stdout_str = stdout_str.split('\n').next().ok_or(Error::FfprobeCheckValidVideoFailed(format!("Failed to get first line: {:?}", stdout_str)))?;
    let stdout_str = stdout_str.trim();
    let mut iter = stdout_str.split(',');

    let width_str = iter.next().ok_or(Error::FfprobeCheckValidVideoFailed(format!("Failed to get width,height: {:?}", stdout_str)))?;
    let height_str = iter.next().ok_or(Error::FfprobeCheckValidVideoFailed(format!("Failed to get width,height: {:?}", stdout_str)))?;
    let width = width_str.parse::<u32>().map_err(|e| Error::FfprobeCheckValidVideoFailed(format!("Failed to parse width ({}): {:?}", width_str, e)))?;
    let height = height_str.parse::<u32>().map_err(|e| Error::FfprobeCheckValidVideoFailed(format!("Failed to parse height ({}): {:?}", height_str, e)))?;

    Ok(width > 0 && height > 0)
}

fn rough_video_secs(video_path: impl AsRef<Path>) -> Result<f64> {
    let video_path = video_path.as_ref();

    let mut command = Command::new("ffprobe");
    command
        .arg("-v").arg("quiet")
        .arg("-show_entries").arg("format=duration")
        .arg("-of").arg("csv=p=0")
        .arg(video_path);
    log::debug!("Command: {:?}", command);
    let output = command.output().map_err(|e| Error::FfprobeShowDurationFailed(format!("{:?}", e)))?;
    log::debug!("Command output: {:?}", output);

    let stdout_str = output.stdout;
    let secs_str = String::from_utf8_lossy(&stdout_str);
    let secs_str = secs_str.trim();
    let secs = secs_str.parse::<f64>().map_err(|e| Error::ParseDurationSecondsFailed(format!("Failed parsed \"{}\": {:?}", secs_str, e)))?;

    Ok(secs)
}

fn hash_file_location(file_path: impl AsRef<Path>) -> String {
    let file_path = file_path.as_ref();
    let file_path_bytes = file_path.as_os_str().as_encoded_bytes();

    let mut hasher = Hasher::new();
    hasher.update(file_path_bytes);
    let hash = hasher.finalize();
    hash.to_hex().to_string()
}

fn destination_filename(path: impl AsRef<Path>, config: &Config) -> Result<String> {
    let path = path.as_ref();
    let filename = if let Some(renamer_config) = &config.renamer {
        renamed_video_filename(path, renamer_config)?
    } else {
        let filename = path.file_name().ok_or(Error::InvalidVideoPath(path.to_path_buf()))?;
        let filename = filename.to_string_lossy();
        filename.to_string()
    };
    Ok(filename)
}

fn renamed_video_filename(path: impl AsRef<Path>, renamer: &RenamerConfig) -> Result<String> {
    let path = path.as_ref();
    let filename = path.file_name().ok_or(Error::InvalidVideoPath(path.to_path_buf()))?;
    let filename = filename.to_string_lossy();
    if filename.chars().count() <= renamer.chars_limit {
        if filename.as_bytes().len() <= renamer.bytes_limit {
            return Ok(filename.to_string());
        }
    }

    let mut command = Command::new(&renamer.command);
    command.args(&renamer.args);
    command.arg(path);

    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(Error::RenamerCommandFailed(stderr, output.status).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if stdout.chars().count() > renamer.chars_limit {
        return Err(Error::TooManyCharsInRenamedFilename(stdout.to_string()).into());
    }
    if stdout.as_bytes().len() > renamer.bytes_limit {
        return Err(Error::TooManyBytesInRenamedFilename(stdout.to_string()).into());
    }
    Ok(stdout.to_string())
}

