//! ffmpeg invocation: same encoding rules as tg-compress.sh.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, fs, thread};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub enum Mode {
    Quality,
    Size,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct Settings {
    pub mode: Mode,
    pub crf: u8,
    pub target_mb: u32,
    /// Cap on the shorter side; 0 keeps the original resolution.
    pub max_res: u32,
    pub speed: String,
    pub audio_kbps: u32,
}

pub fn presets() -> Vec<(&'static str, Settings)> {
    let quality = |crf, max_res, audio_kbps| Settings {
        mode: Mode::Quality,
        crf,
        target_mb: 50,
        max_res,
        speed: "slow".into(),
        audio_kbps,
    };
    let size = |target_mb| Settings {
        mode: Mode::Size,
        crf: 26,
        target_mb,
        max_res: 720,
        speed: "slow".into(),
        audio_kbps: 128,
    };
    vec![
        ("Balanced · 720p", quality(26, 720, 128)),
        ("High quality · 1080p", quality(23, 1080, 160)),
        ("Small · 480p", quality(30, 480, 96)),
        ("Fit under 50 MB (bot upload)", size(50)),
        ("Fit under 20 MB", size(20)),
    ]
}

pub enum Failure {
    Cancelled,
    Error(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Self {
        Failure::Error(e)
    }
}

#[derive(Clone)]
pub struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl Tools {
    pub fn find() -> Option<Self> {
        Some(Self {
            ffmpeg: find_tool("ffmpeg")?,
            ffprobe: find_tool("ffprobe")?,
        })
    }

    fn probe(&self, input: &Path, args: &[&str]) -> Result<String, String> {
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error"])
            .args(args)
            .args(["-of", "csv=p=0"])
            .arg(input)
            .output()
            .map_err(|e| format!("Couldn't run ffprobe: {e}"))?;
        if !out.status.success() {
            return Err(last_line(&String::from_utf8_lossy(&out.stderr), "Not a readable video"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn run(
        &self,
        args: &[String],
        duration: f64,
        cancel: &AtomicBool,
        mut progress: impl FnMut(f32),
    ) -> Result<(), Failure> {
        let mut child = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;

        // Drain stderr on its own thread so a chatty ffmpeg can't block on a full pipe.
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let stderr_thread = thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        // `-progress pipe:1` writes key=value lines about twice a second.
        for line in BufReader::new(child.stdout.take().expect("stdout is piped")).lines() {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }
            let Ok(line) = line else { break };
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
                progress((us / 1e6 / duration).clamp(0.0, 1.0) as f32);
            }
        }

        let status = child.wait().map_err(|e| e.to_string())?;
        let stderr = stderr_thread.join().unwrap_or_default();
        if cancel.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        if !status.success() {
            return Err(last_line(&stderr, &format!("ffmpeg failed ({status})")).into());
        }
        Ok(())
    }
}

/// GUI apps launched from Finder don't inherit the shell PATH, so also check Homebrew's dirs.
fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let mut dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

fn last_line(text: &str, fallback: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or_else(|| fallback.to_string(), |l| l.trim().to_string())
}

/// `<dir>/<stem><suffix>.mp4`, numbered if that file already exists, so nothing is overwritten.
pub fn unique_output(dir: &Path, input: &Path, suffix: &str) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let mut path = dir.join(format!("{stem}{suffix}.mp4"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}{suffix}-{n}.mp4"));
        n += 1;
    }
    path
}

struct Info {
    duration: f64,
    has_audio: bool,
}

impl Tools {
    fn info(&self, input: &Path) -> Result<Info, String> {
        let duration = self
            .probe(input, &["-show_entries", "format=duration"])?
            .parse()
            .map_err(|_| "Couldn't read the video's duration".to_string())?;
        let has_audio = !self
            .probe(input, &["-select_streams", "a:0", "-show_entries", "stream=index"])?
            .is_empty();
        Ok(Info { duration, has_audio })
    }
}

/// Video encoder args shared by the real encode and the size-estimate samples.
fn video_args(s: &Settings) -> Vec<String> {
    // Shorter side capped at max_res; even dimensions are required by yuv420p.
    let vf = if s.max_res > 0 {
        let r = s.max_res;
        format!("scale='if(gt(iw,ih),-2,min({r},iw))':'if(gt(iw,ih),min({r},ih),-2)',format=yuv420p")
    } else {
        "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p".to_string()
    };
    [
        "-map", "0:v:0", "-vf", &vf, "-c:v", "libx264", "-preset", &s.speed, "-profile:v", "high",
        "-pix_fmt", "yuv420p",
    ]
    .map(String::from)
    .to_vec()
}

/// Size mode bitrates (video, audio) in kbps, with ~3% headroom for container overhead.
fn size_budget(s: &Settings, info: &Info) -> Result<(i64, i64), String> {
    let total_kbps = (s.target_mb as f64 * 8192.0 * 0.97 / info.duration) as i64;
    let audio_kbps = if info.has_audio { s.audio_kbps as i64 } else { 0 };
    let video_kbps = total_kbps - audio_kbps;
    if video_kbps < 100 {
        return Err(format!(
            "{} MB is too small for a {:.0}s video ({video_kbps} kbps)",
            s.target_mb, info.duration
        ));
    }
    Ok((video_kbps, audio_kbps))
}

/// Expected output size in bytes. In size mode it is the bitrate budget, an upper bound (simple
/// footage can come in under it); quality mode encodes a few short samples with the real
/// settings and extrapolates their bitrate.
pub fn estimate(tools: &Tools, input: &Path, s: &Settings) -> Result<u64, String> {
    let info = tools.info(input)?;
    if s.mode == Mode::Size {
        let (video_kbps, audio_kbps) = size_budget(s, &info)?;
        return Ok(((video_kbps + audio_kbps) as f64 * 125.0 * info.duration) as u64);
    }

    const SAMPLE: f64 = 6.0;
    let samples: Vec<(f64, f64)> = if info.duration <= SAMPLE * 3.0 + 2.0 {
        vec![(0.0, info.duration)]
    } else {
        [0.15, 0.5, 0.85].map(|f| (f * info.duration - SAMPLE / 2.0, SAMPLE)).to_vec()
    };

    let (mut bytes, mut seconds) = (0usize, 0.0);
    for (start, len) in samples {
        let out = Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-ss", &start.to_string(), "-t", &len.to_string(), "-i"])
            .arg(input)
            .args(video_args(s))
            .args(["-crf", &s.crf.to_string(), "-an", "-f", "h264", "-"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("Couldn't run ffmpeg: {e}"))?;
        if !out.status.success() {
            return Err(last_line(&String::from_utf8_lossy(&out.stderr), "Estimate failed"));
        }
        bytes += out.stdout.len();
        seconds += len;
    }

    let audio = if info.has_audio { s.audio_kbps as f64 * 125.0 * info.duration } else { 0.0 };
    Ok((bytes as f64 / seconds * info.duration + audio) as u64)
}

/// Encode `input` to `output`, reporting overall progress (0..1) and the current stage.
pub fn compress(
    tools: &Tools,
    input: &Path,
    output: &Path,
    s: &Settings,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32, &str),
) -> Result<(), Failure> {
    let info = tools.info(input)?;
    let (duration, has_audio) = (info.duration, info.has_audio);

    let mut common: Vec<String> = [
        "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-y", "-i",
    ]
    .map(String::from)
    .to_vec();
    common.push(input.to_string_lossy().into_owned());
    common.extend(video_args(s));
    common.extend(["-map", "0:a:0?"].map(String::from));
    let audio: Vec<String> = if has_audio {
        let kbps = format!("{}k", s.audio_kbps);
        ["-c:a", "aac", "-b:a", &kbps, "-ac", "2"].map(String::from).to_vec()
    } else {
        vec!["-an".into()]
    };
    let finish = |args: &mut Vec<String>| {
        args.extend(["-movflags", "+faststart"].map(String::from));
        args.push(output.to_string_lossy().into_owned());
    };

    match s.mode {
        Mode::Quality => {
            let mut args = common;
            args.extend(["-crf".into(), s.crf.to_string()]);
            args.extend(audio);
            finish(&mut args);
            tools.run(&args, duration, cancel, |p| progress(p, "Encoding"))
        }
        Mode::Size => {
            let (video_kbps, _) = size_budget(s, &info)?;

            let pass_dir = env::temp_dir().join(format!("tg-compress-{}", std::process::id()));
            fs::create_dir_all(&pass_dir).map_err(|e| e.to_string())?;
            let passlog = pass_dir.join("pass").to_string_lossy().into_owned();
            let rate = |pass: &str| -> Vec<String> {
                vec![
                    "-b:v".into(), format!("{video_kbps}k"),
                    "-pass".into(), pass.into(),
                    "-passlogfile".into(), passlog.clone(),
                ]
            };

            let mut pass1 = common.clone();
            pass1.extend(rate("1"));
            pass1.extend(["-an", "-f", "mp4", if cfg!(windows) { "NUL" } else { "/dev/null" }].map(String::from));

            let mut pass2 = common;
            pass2.extend(rate("2"));
            pass2.extend([
                "-maxrate".into(), format!("{}k", video_kbps * 3 / 2),
                "-bufsize".into(), format!("{}k", video_kbps * 2),
            ]);
            pass2.extend(audio);
            finish(&mut pass2);

            let result = tools
                .run(&pass1, duration, cancel, |p| progress(p * 0.5, "Pass 1/2"))
                .and_then(|()| tools.run(&pass2, duration, cancel, |p| progress(0.5 + p * 0.5, "Pass 2/2")));
            let _ = fs::remove_dir_all(&pass_dir);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_in_both_modes() {
        let tools = Tools::find().expect("ffmpeg installed");
        let dir = env::temp_dir().join(format!("tg-compress-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("sample.mp4");
        let ok = Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30:duration=10"])
            .args(["-f", "lavfi", "-i", "sine=duration=10", "-c:v", "libx264", "-crf", "10", "-c:a", "aac"])
            .arg(&input)
            .status()
            .unwrap()
            .success();
        assert!(ok);

        let cancel = AtomicBool::new(false);
        for (name, settings) in presets() {
            let output = unique_output(&dir, &input, "_tg");
            let estimated = estimate(&tools, &input, &settings).unwrap_or_else(|e| panic!("{name}: {e}"));
            let mut last = 0.0;
            let result = compress(&tools, &input, &output, &settings, &cancel, |p, _| last = p);
            assert!(result.is_ok(), "{name} failed");
            let size = fs::metadata(&output).unwrap().len();
            if settings.mode == Mode::Quality {
                let error = (estimated as f64 / size as f64 - 1.0).abs();
                assert!(error < 0.25, "{name}: estimated {estimated}, got {size}");
            } else {
                assert!(size <= estimated * 21 / 20, "{name}: budget {estimated}, got {size}");
            }
            if settings.mode == Mode::Size {
                assert!(size <= settings.target_mb as u64 * 1024 * 1024, "{name}: {size} bytes");
            }
            assert!(last > 0.9, "{name}: progress only reached {last}");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
