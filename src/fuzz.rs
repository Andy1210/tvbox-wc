//! Entry points for fuzzing the control socket's decoding. Not an API.
//!
//! Everything a peer's bytes pass through before they reach the compositor's own
//! state: framing into lines, the line cap, decoding a request, the checks a
//! request's arguments get before they are acted on, and encoding the reply. The
//! compositor runs in the session's only process that draws the screen, so a panic
//! anywhere on this path is a dead television, and none may be reachable from a
//! connection.

use crate::ipc::{self, Parsed, Request};

/// Feed a byte stream through the control socket's decoding, the way reads would
/// deliver it, and check every reply is one line of JSON.
pub fn control_stream(data: &[u8]) {
    // The first byte picks the read size, so the same input also exercises lines
    // split across reads.
    let chunk = data.first().map_or(1, |byte| usize::from(*byte % 64) + 1);
    let mut buffer = Vec::new();
    for piece in data.chunks(chunk) {
        buffer.extend_from_slice(piece);
        while let Some(line) = ipc::next_line(&mut buffer) {
            check_line(&line);
        }
        if ipc::unterminated_too_long(&buffer) {
            // The socket drops the connection here; a fresh one starts empty.
            buffer.clear();
        }
    }
}

/// Decode one line and check its replies.
pub fn check_line(line: &[u8]) {
    match ipc::parse_line(line) {
        Parsed::Blank => {}
        Parsed::Invalid(reply) => check_reply(&reply),
        Parsed::Request(id, request) => {
            if let Request::PlaceWindow {
                app_id,
                title,
                x,
                y,
                w,
                h,
            } = request
            {
                if let Ok((_, Some(rect))) = ipc::place_target(app_id, title, x, y, w, h) {
                    assert!(rect.size.w > 0 && rect.size.h > 0);
                }
            }
            check_reply(&ipc::reply(id, Ok(serde_json::Value::Null)));
            check_reply(&ipc::reply(
                id,
                Err(anyhow::anyhow!("refused: \u{0}\n\"quoted\"")),
            ));
        }
    }
}

fn check_reply(reply: &str) {
    assert!(!reply.contains('\n'), "a reply must be one line: {reply:?}");
    let value: serde_json::Value = serde_json::from_str(reply).expect("a reply must be JSON");
    assert!(value.is_object(), "a reply must be an object: {reply:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests the shell really sends, as the seeds for mutation.
    const SEEDS: &[&str] = &[
        r#"{"id":1,"request":"get_outputs"}"#,
        r#"{"id":2,"request":"set_mode","output":"HDMI-A-1","w":1920,"h":1080}"#,
        r#"{"id":3,"request":"set_mode","output":"HDMI-A-1","w":1920,"h":1080,"refresh":59940}"#,
        r#"{"id":4,"request":"get_state"}"#,
        r#"{"id":5,"request":"place_window","app_id":"mpv","x":10,"y":10,"w":640,"h":360}"#,
        r#"{"id":6,"request":"place_window","title":"overlay"}"#,
        r#"{"id":7,"request":"type_text","text":"héllo\nworld","select_all":true}"#,
        r#"{"id":8,"request":"screenshot","path":"/run/user/1000/s.png"}"#,
        r#"{"id":9,"request":"set_focus","owner":"app","app":"player"}"#,
        r#"{"id":10,"request":"set_hdr","output":"HDMI-A-1","on":true}"#,
    ];

    /// A small deterministic generator, so a failure reproduces from its seed.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn mutate(rng: &mut XorShift, seed: &[u8]) -> Vec<u8> {
        let mut out = seed.to_vec();
        for _ in 0..=rng.below(4) {
            match rng.below(6) {
                // Flip a byte.
                0 if !out.is_empty() => {
                    let at = rng.below(out.len());
                    out[at] = rng.next() as u8;
                }
                // Cut it short.
                1 if !out.is_empty() => out.truncate(rng.below(out.len())),
                // Splice in another request.
                2 => {
                    let other = SEEDS[rng.below(SEEDS.len())].as_bytes();
                    let at = rng.below(out.len() + 1);
                    out.splice(at..at, other.iter().copied());
                }
                // Swap a number for one that does not fit.
                3 => {
                    let text = String::from_utf8_lossy(&out).into_owned();
                    out = text
                        .replacen("1920", "99999999999999999999", 1)
                        .replacen("1080", "-2147483649", 1)
                        .into_bytes();
                }
                // Deep nesting.
                4 => {
                    let depth = rng.below(4096);
                    let at = rng.below(out.len() + 1);
                    out.splice(at..at, std::iter::repeat_n(b'[', depth));
                }
                // A newline or a NUL somewhere.
                _ => {
                    let at = rng.below(out.len() + 1);
                    out.insert(at, if rng.below(2) == 0 { b'\n' } else { 0 });
                }
            }
        }
        out
    }

    #[test]
    fn the_shells_requests_decode() {
        for seed in SEEDS {
            assert!(
                matches!(ipc::parse_line(seed.as_bytes()), Parsed::Request(..)),
                "{seed}"
            );
            check_line(seed.as_bytes());
        }
    }

    #[test]
    fn garbage_on_the_socket_never_panics() {
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        for _ in 0..5_000 {
            let input = if rng.below(4) == 0 {
                (0..rng.below(512)).map(|_| rng.next() as u8).collect()
            } else {
                let seed = SEEDS[rng.below(SEEDS.len())].as_bytes();
                let mut line = mutate(&mut rng, seed);
                line.push(b'\n');
                line
            };
            control_stream(&input);
        }
    }

    #[test]
    fn an_endless_line_is_cut_off_and_the_stream_recovers() {
        let mut input = vec![63u8]; // 64-byte reads
        input.extend(std::iter::repeat_n(b'x', ipc::MAX_LINE + 10));
        input.push(b'\n');
        input.extend_from_slice(SEEDS[0].as_bytes());
        input.push(b'\n');
        control_stream(&input);
    }

    #[test]
    fn a_rectangle_needs_a_positive_size() {
        assert!(
            ipc::place_target(Some("mpv".into()), None, Some(0), Some(0), Some(0), Some(5))
                .is_err()
        );
        assert!(
            ipc::place_target(Some("mpv".into()), None, Some(0), None, Some(1), Some(1)).is_err()
        );
        assert!(
            ipc::place_target(Some("mpv".into()), Some("t".into()), None, None, None, None)
                .is_err()
        );
        assert!(ipc::place_target(None, None, None, None, None, None).is_err());
        assert!(
            ipc::place_target(Some("mpv".into()), None, None, None, None, None)
                .unwrap()
                .1
                .is_none()
        );
    }
}
