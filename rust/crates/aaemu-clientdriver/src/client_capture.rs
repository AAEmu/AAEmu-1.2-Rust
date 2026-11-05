//! Tool definitions and argument handling for the native skill trace (`client_trace`) and the
//! constant-frame-rate video capture (`client_video`). The recording itself lives in the
//! injected driver (`native/skilltrace.cpp`, `native/video.cpp`).
use super::tool;
#[cfg(any(windows, test))]
use anyhow::{bail, Context};
use serde_json::{json, Value};

/// Trace row kinds in the order the driver reports them. `frame` rows are one per rendered
/// frame, so the driver only writes them when asked.
#[cfg(any(windows, test))]
pub const TRACE_KINDS: [&str; 5] = ["anim", "fx", "sound", "press", "frame"];

pub fn tools(pid: Value) -> Vec<Value> {
    vec![
        tool("client_trace", "Record the original client's animation, particle, sound, key-press and frame events as skillparity/v1 JSON rows (src \"orig\", Unix-microsecond wall_us, Present counter frame) in a bounded in-process ring buffer. start installs observing hooks after verifying the exact r208022 engine build (a hook whose build check fails stays inert and is reported); drain returns the rows recorded since the last drain as JSONL; stop restores the hooks and keeps unread rows for one more drain; status returns {running, rows, dropped, ...}. kinds defaults to anim, fx, sound and press; add frame for one row per rendered frame. Only observes: never changes game state.",
            json!({"pid":pid.clone(),"action":{"type":"string","enum":["start","drain","stop","status"]},
                "kinds":{"type":"array","maxItems":5,"uniqueItems":true,"items":{"type":"string","enum":["anim","fx","sound","press","frame"]},"description":"Row kinds to record; only with start. Default anim, fx, sound, press."}}), &["action"], false),
        tool("client_video", "Record the client's rendered back buffer to an H.264 mp4 at a constant frame rate tied to wall time: frame k of the file shows the most recent game frame at t0 + k/fps (game frames are repeated or dropped to keep the cadence). Needs the DX10/DX11 renderer and ffmpeg. start spawns ffmpeg and returns once the first frame was seen; stop finishes the file and writes <path>.json with {t0_wall_us, fps, width, height, frames, repeated, dropped}. The render thread never waits for the encoder: when ffmpeg falls behind, frames are dropped and counted.",
            json!({"pid":pid,"action":{"type":"string","enum":["start","stop","status"]},
                "path":{"type":"string","minLength":1,"maxLength":32767,"description":"Output .mp4/.mkv/.mov; only with start. Relative paths resolve against the MCP server's working directory."},
                "fps":{"type":"integer","minimum":1,"maximum":120,"default":30,"description":"Output frame rate; only with start. Default 30."},
                "ffmpeg":{"type":"string","minLength":1,"maxLength":32767,"description":"Path to ffmpeg.exe; only with start. Default: ffmpeg on the client's PATH."}}), &["action"], false),
    ]
}

/// The payload of the driver's `R` request for a validated `client_trace` call.
#[cfg(any(windows, test))]
pub fn trace_command(args: &Value) -> anyhow::Result<String> {
    let action = args["action"].as_str().context("missing action")?;
    let kinds = args.get("kinds");
    match action {
        "start" => {
            let Some(kinds) = kinds else {
                return Ok("start".into());
            };
            let kinds = kinds.as_array().context("kinds must be an array")?;
            if kinds.is_empty() {
                bail!("kinds must not be empty (omit it for the default kinds)");
            }
            let mut chosen = Vec::new();
            for kind in TRACE_KINDS {
                if kinds.iter().any(|k| k.as_str() == Some(kind)) {
                    chosen.push(kind);
                }
            }
            if chosen.len() != kinds.len() {
                bail!("kinds must be distinct values of anim, fx, sound, press, frame");
            }
            Ok(format!("start {}", chosen.join(",")))
        }
        "drain" | "stop" | "status" => {
            if kinds.is_some() {
                bail!("kinds applies only to action start");
            }
            Ok(action.into())
        }
        _ => bail!("trace action must be start, drain, stop or status"),
    }
}

/// The payload of the driver's `M` request, and the absolute output path for `start`.
/// `cwd` anchors a relative path (the driver runs inside the client, whose directory is not
/// the caller's).
#[cfg(any(windows, test))]
pub fn video_command(
    args: &Value,
    cwd: &std::path::Path,
) -> anyhow::Result<(String, Option<std::path::PathBuf>)> {
    let action = args["action"].as_str().context("missing action")?;
    match action {
        "start" => {
            let path = args["path"]
                .as_str()
                .context("start needs the output path")?;
            let fps = match args.get("fps") {
                Some(v) => v.as_u64().context("fps must be an integer")?,
                None => 30,
            };
            if !(1..=120).contains(&fps) {
                bail!("fps must be 1..120");
            }
            let ffmpeg = match args.get("ffmpeg") {
                Some(v) => v.as_str().context("ffmpeg must be a string")?,
                None => "",
            };
            for (name, text) in [("path", path), ("ffmpeg", ffmpeg)] {
                if text.contains(['"', '\n', '\r', '\0']) {
                    bail!("{name} must not contain quotes or line breaks");
                }
            }
            let lower = path.to_ascii_lowercase();
            if ![".mp4", ".mkv", ".mov"]
                .iter()
                .any(|ext| lower.ends_with(ext))
            {
                bail!("path must end in .mp4, .mkv or .mov");
            }
            let mut full = std::path::PathBuf::from(path);
            if !full.is_absolute() {
                full = cwd.join(full);
            }
            let text = full.to_str().context("path is not valid UTF-8")?;
            Ok((format!("start\n{fps}\n{ffmpeg}\n{text}"), Some(full)))
        }
        "stop" | "status" => {
            if ["path", "fps", "ffmpeg"]
                .iter()
                .any(|k| args.get(k).is_some())
            {
                bail!("path, fps and ffmpeg apply only to action start");
            }
            Ok((action.into(), None))
        }
        _ => bail!("video action must be start, stop or status"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(name: &str) -> Value {
        super::super::tools()
            .into_iter()
            .find(|t| t["name"] == name)
            .unwrap()
    }
    fn valid(name: &str, args: Value) -> bool {
        super::super::validate(&definition(name)["inputSchema"], &args).is_ok()
    }

    #[test]
    fn trace_schema_accepts_the_documented_calls_and_rejects_the_rest() {
        for args in [
            json!({"action":"start"}),
            json!({"action":"start","kinds":["anim","fx","sound","press","frame"]}),
            json!({"action":"start","kinds":["frame"],"pid":4}),
            json!({"action":"drain"}),
            json!({"action":"stop"}),
            json!({"action":"status","pid":1}),
        ] {
            assert!(valid("client_trace", args.clone()), "{args}");
        }
        for args in [
            json!({}),
            json!({"action":"pause"}),
            json!({"action":1}),
            json!({"action":"start","kinds":"anim"}),
            json!({"action":"start","kinds":["mark"]}),
            json!({"action":"start","kinds":["anim","anim"]}),
            json!({"action":"start","kinds":["anim","fx","sound","press","frame","anim"]}),
            json!({"action":"start","kinds":[1]}),
            json!({"action":"start","path":"x"}),
            json!({"action":"drain","pid":0}),
        ] {
            assert!(!valid("client_trace", args.clone()), "{args}");
        }
        assert_eq!(
            definition("client_trace")["annotations"]["readOnlyHint"],
            false
        );
    }

    #[test]
    fn trace_commands_are_canonical_and_kinds_only_go_with_start() {
        assert_eq!(trace_command(&json!({"action":"start"})).unwrap(), "start");
        // the order the caller gives does not matter; the driver gets the canonical list
        assert_eq!(
            trace_command(&json!({"action":"start","kinds":["frame","fx","anim"]})).unwrap(),
            "start anim,fx,frame"
        );
        assert_eq!(
            trace_command(&json!({"action":"start","kinds":["press"]})).unwrap(),
            "start press"
        );
        for action in ["drain", "stop", "status"] {
            assert_eq!(trace_command(&json!({"action":action})).unwrap(), action);
            assert!(trace_command(&json!({"action":action,"kinds":["anim"]})).is_err());
        }
        assert!(trace_command(&json!({"action":"start","kinds":[]})).is_err());
        assert!(trace_command(&json!({"action":"start","kinds":["anim","anim"]})).is_err());
        assert!(trace_command(&json!({"action":"start","kinds":["bogus"]})).is_err());
        assert!(trace_command(&json!({"action":"restart"})).is_err());
    }

    #[test]
    fn video_schema_bounds_fps_and_strings() {
        for args in [
            json!({"action":"start","path":"D:/caps/run.mp4"}),
            json!({"action":"start","path":"a.mp4","fps":120,"ffmpeg":"C:/tools/ffmpeg.exe"}),
            json!({"action":"start","path":"a.mp4","fps":1}),
            json!({"action":"stop"}),
            json!({"action":"status","pid":9}),
        ] {
            assert!(valid("client_video", args.clone()), "{args}");
        }
        for args in [
            json!({}),
            json!({"action":"pause"}),
            json!({"action":"start","path":""}),
            json!({"action":"start","path":"a.mp4","fps":0}),
            json!({"action":"start","path":"a.mp4","fps":121}),
            json!({"action":"start","path":"a.mp4","fps":30.5}),
            json!({"action":"start","path":"a.mp4","fps":"30"}),
            json!({"action":"start","path":"a.mp4","ffmpeg":""}),
            json!({"action":"start","path":"a.mp4","fmt":"gif"}),
            json!({"action":"start","path":"x".repeat(32768)}),
        ] {
            assert!(!valid("client_video", args.clone()), "{args}");
        }
        assert_eq!(
            definition("client_video")["inputSchema"]["properties"]["fps"]["default"],
            30
        );
    }

    #[test]
    fn video_commands_resolve_paths_and_reject_unsafe_input() {
        let cwd = std::env::temp_dir().join("caps");
        let absolute = cwd.join("run").join("out.mp4");
        let text = absolute.to_str().unwrap().to_string();
        // an absolute path is kept; fps defaults to 30; no ffmpeg means "on PATH" (empty line)
        let (command, path) = video_command(&json!({"action":"start","path":text}), &cwd).unwrap();
        assert_eq!(command, format!("start\n30\n\n{text}"));
        assert_eq!(path.unwrap(), absolute);
        // a relative path is anchored at the caller's directory, not the client's
        let (command, path) = video_command(
            &json!({"action":"start","path":"out/Run.MP4","fps":60,"ffmpeg":"tools/ffmpeg.exe"}),
            &cwd,
        )
        .unwrap();
        let joined = cwd.join("out/Run.MP4");
        assert_eq!(
            command,
            format!("start\n60\ntools/ffmpeg.exe\n{}", joined.to_str().unwrap())
        );
        assert_eq!(path.unwrap(), joined);
        for ext in ["a.mkv", "a.MOV"] {
            assert!(video_command(&json!({"action":"start","path":ext}), &cwd).is_ok());
        }
        for bad in [
            json!({"action":"start"}),
            json!({"action":"start","path":"a.gif"}),
            json!({"action":"start","path":"noext"}),
            json!({"action":"start","path":"a\"b.mp4"}),
            json!({"action":"start","path":"a\nb.mp4"}),
            json!({"action":"start","path":"a.mp4","ffmpeg":"x\" -i evil"}),
            json!({"action":"start","path":"a.mp4","ffmpeg":"x\ny"}),
            json!({"action":"start","path":"a.mp4","fps":0}),
            json!({"action":"start","path":"a.mp4","fps":121}),
            json!({"action":"start","path":"a.mp4","fps":"fast"}),
            json!({"action":"pause"}),
        ] {
            assert!(video_command(&bad, &cwd).is_err(), "{bad}");
        }
        for action in ["stop", "status"] {
            assert_eq!(
                video_command(&json!({"action":action}), &cwd).unwrap(),
                (action.to_string(), None)
            );
            for extra in [
                json!({"action":action,"path":"a.mp4"}),
                json!({"action":action,"fps":30}),
                json!({"action":action,"ffmpeg":"ffmpeg"}),
            ] {
                assert!(video_command(&extra, &cwd).is_err(), "{extra}");
            }
        }
    }
}
