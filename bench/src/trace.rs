// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Trace-contract parsing for `bench --stream`.
//!
//! Implements the consumer side of `bench/trace-format.md` (issue #47):
//!
//! - Block events: `{"ts_ms": int, "height": int, "tx_count": int|null}`
//! - Gap markers: `{"gap": true, ...}` -- skipped and counted, never
//!   enqueued (skip-and-count policy, spec section 3).
//! - Provenance header: first line with a top-level `"provenance"` key
//!   -- parsed, logged, skipped. Headerless traces are accepted as
//!   pre-spec captures (spec section 4).
//! - Monotonicity (spec section 5): `ts_ms` non-decreasing, `height`
//!   strictly increasing across block events. Violations are fatal:
//!   the source stops and records the error so the caller can exit
//!   non-zero.
//! - Unknown top-level keys are ignored (forward compatibility).
//!
//! No plonky2 / circuit dependencies: everything here is unit-testable
//! with plain std + serde_json.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use log::{info, warn};

use crate::stream::StreamShared;

/// A single block event from the trace, per the contract schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEvent {
    /// Epoch milliseconds at which the block was observed/scheduled.
    pub ts_ms: i64,
    /// L2 block height.
    pub height: u64,
    /// Transactions in the block; `None` for recorder nulls. Replayed
    /// input should have these filled by the producer (P1), but the
    /// consumer stays lenient (treats `None` as 500 downstream).
    pub tx_count: Option<u64>,
}

/// The minimal source seam for the streaming bench. v1 has exactly one
/// implementation (`LineSource` over stdin); a future Pub/Sub source
/// (issue #50) plugs in here.
pub trait TraceSource {
    /// Blocking. Returns the next block event, or `None` on end of
    /// stream (EOF or fatal contract violation -- check the shared
    /// `fatal` slot to distinguish).
    fn next_event(&mut self) -> Option<TraceEvent>;
}

/// Classification of a single trace line.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedLine {
    /// Provenance header (`{"provenance": ...}`). Carries the raw
    /// provenance JSON for logging.
    Header(serde_json::Value),
    /// A block event.
    Block(TraceEvent),
    /// A gap marker (`{"gap": true, ...}`).
    Gap { ts_ms: Option<i64>, reason: String },
    /// Anything that does not parse per the contract. Carries a
    /// human-readable reason.
    Malformed(String),
}

/// Parse one trace line per the contract. Never panics.
pub fn parse_trace_line(line: &str) -> ParsedLine {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return ParsedLine::Malformed(format!("invalid JSON: {e}")),
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => return ParsedLine::Malformed("not a JSON object".to_string()),
    };

    // Discriminator order per spec section 1.
    if let Some(prov) = obj.get("provenance") {
        return ParsedLine::Header(prov.clone());
    }
    if obj.get("gap").and_then(|g| g.as_bool()) == Some(true) {
        return ParsedLine::Gap {
            ts_ms: obj.get("ts_ms").and_then(|v| v.as_i64()),
            reason: obj
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        };
    }
    if obj.contains_key("height") {
        let ts_ms = match obj.get("ts_ms").and_then(|v| v.as_i64()) {
            Some(t) => t,
            None => return ParsedLine::Malformed("block event missing int ts_ms".to_string()),
        };
        let height = match obj.get("height").and_then(|v| v.as_u64()) {
            Some(h) => h,
            None => return ParsedLine::Malformed("block event height not a u64".to_string()),
        };
        let tx_count = match obj.get("tx_count") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n),
                // P4: tx_count is int|null, never a float.
                None => {
                    return ParsedLine::Malformed(format!(
                        "tx_count must be int or null, got {v}"
                    ));
                }
            },
        };
        return ParsedLine::Block(TraceEvent { ts_ms, height, tx_count });
    }
    ParsedLine::Malformed("unrecognized line type (no provenance/gap/height key)".to_string())
}

/// Blocking line-oriented `TraceSource` over any `BufRead` (v1: stdin).
///
/// Per the contract it:
/// - parses/logs/skips the provenance header (and accepts headerless
///   pre-spec traces),
/// - skips and counts gap markers,
/// - warns on and counts malformed lines,
/// - enforces monotonicity, recording a fatal error in `shared.fatal`
///   and requesting shutdown on violation.
pub struct LineSource<R: BufRead> {
    reader: R,
    shared: Arc<StreamShared>,
    line_no: u64,
    seen_header: bool,
    last_ts_ms: Option<i64>,
    last_height: Option<u64>,
    done: bool,
}

impl<R: BufRead> LineSource<R> {
    pub fn new(reader: R, shared: Arc<StreamShared>) -> Self {
        Self {
            reader,
            shared,
            line_no: 0,
            seen_header: false,
            last_ts_ms: None,
            last_height: None,
            done: false,
        }
    }

    fn fatal(&mut self, msg: String) {
        warn!("trace: fatal contract violation: {msg}");
        self.shared.set_fatal(msg);
        self.done = true;
    }
}

impl<R: BufRead> TraceSource for LineSource<R> {
    fn next_event(&mut self) -> Option<TraceEvent> {
        if self.done {
            return None;
        }
        loop {
            if self.shared.shutdown_requested() {
                return None;
            }
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None, // EOF
                Ok(_) => {}
                Err(e) => {
                    warn!("trace: read error, treating as end of stream: {e}");
                    return None;
                }
            }
            self.line_no += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // Blank lines are forbidden by the spec; be lenient on
                // read but count as malformed.
                self.shared.malformed_lines.fetch_add(1, Ordering::Relaxed);
                warn!("trace: line {}: blank line (forbidden by spec)", self.line_no);
                continue;
            }
            match parse_trace_line(trimmed) {
                ParsedLine::Header(prov) => {
                    if self.seen_header || self.line_no != 1 {
                        warn!(
                            "trace: line {}: unexpected extra/late provenance header; skipping",
                            self.line_no
                        );
                        self.shared.malformed_lines.fetch_add(1, Ordering::Relaxed);
                    } else {
                        info!("trace: provenance header: {prov}");
                    }
                    self.seen_header = true;
                    continue;
                }
                ParsedLine::Gap { ts_ms, reason } => {
                    self.shared.gaps_skipped.fetch_add(1, Ordering::Relaxed);
                    info!(
                        "trace: line {}: gap marker (reason={reason}, ts_ms={ts_ms:?}); skip-and-count",
                        self.line_no
                    );
                    continue;
                }
                ParsedLine::Malformed(reason) => {
                    self.shared.malformed_lines.fetch_add(1, Ordering::Relaxed);
                    warn!("trace: line {}: malformed line skipped: {reason}", self.line_no);
                    continue;
                }
                ParsedLine::Block(ev) => {
                    // Monotonicity (spec section 5): hard errors.
                    if let Some(last_ts) = self.last_ts_ms {
                        if ev.ts_ms < last_ts {
                            self.fatal(format!(
                                "line {}: ts_ms regressed ({} -> {})",
                                self.line_no, last_ts, ev.ts_ms
                            ));
                            return None;
                        }
                    }
                    if let Some(last_h) = self.last_height {
                        if ev.height <= last_h {
                            self.fatal(format!(
                                "line {}: height not strictly increasing ({} -> {})",
                                self.line_no, last_h, ev.height
                            ));
                            return None;
                        }
                    }
                    self.last_ts_ms = Some(ev.ts_ms);
                    self.last_height = Some(ev.height);
                    return Some(ev);
                }
            }
        }
    }
}

/// Convenience constructor: a `LineSource` over this process's stdin.
pub fn stdin_source(shared: Arc<StreamShared>) -> LineSource<std::io::BufReader<std::io::Stdin>> {
    LineSource::new(std::io::BufReader::new(std::io::stdin()), shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn shared() -> Arc<StreamShared> {
        Arc::new(StreamShared::new())
    }

    fn drain<R: BufRead>(src: &mut LineSource<R>) -> Vec<TraceEvent> {
        let mut out = Vec::new();
        while let Some(ev) = src.next_event() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn parse_block_event() {
        let p = parse_trace_line(r#"{"ts_ms": 1781143874390, "height": 260138266, "tx_count": 500}"#);
        assert_eq!(
            p,
            ParsedLine::Block(TraceEvent {
                ts_ms: 1_781_143_874_390,
                height: 260_138_266,
                tx_count: Some(500)
            })
        );
    }

    #[test]
    fn parse_null_tx_count() {
        let p = parse_trace_line(r#"{"ts_ms": 1, "height": 2, "tx_count": null}"#);
        assert_eq!(
            p,
            ParsedLine::Block(TraceEvent { ts_ms: 1, height: 2, tx_count: None })
        );
        // Missing tx_count treated the same as explicit null.
        let p = parse_trace_line(r#"{"ts_ms": 1, "height": 2}"#);
        assert_eq!(
            p,
            ParsedLine::Block(TraceEvent { ts_ms: 1, height: 2, tx_count: None })
        );
    }

    #[test]
    fn parse_gap_marker() {
        let p = parse_trace_line(
            r#"{"gap": true, "ts_ms": 1781143880000, "reason": "ws_disconnect"}"#,
        );
        assert_eq!(
            p,
            ParsedLine::Gap { ts_ms: Some(1_781_143_880_000), reason: "ws_disconnect".into() }
        );
    }

    #[test]
    fn parse_provenance_header() {
        let p = parse_trace_line(
            r#"{"provenance": {"generator": "synth-peak", "params": {}, "generated_at": "2026-06-11T08:00:00Z"}}"#,
        );
        assert!(matches!(p, ParsedLine::Header(_)));
    }

    #[test]
    fn parse_malformed_lines() {
        assert!(matches!(parse_trace_line("not json"), ParsedLine::Malformed(_)));
        assert!(matches!(parse_trace_line("[1,2,3]"), ParsedLine::Malformed(_)));
        assert!(matches!(parse_trace_line(r#"{"foo": 1}"#), ParsedLine::Malformed(_)));
        // Missing ts_ms.
        assert!(matches!(
            parse_trace_line(r#"{"height": 5}"#),
            ParsedLine::Malformed(_)
        ));
        // P4: float tx_count is a contract violation.
        assert!(matches!(
            parse_trace_line(r#"{"ts_ms": 1, "height": 2, "tx_count": 400.72}"#),
            ParsedLine::Malformed(_)
        ));
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let p = parse_trace_line(r#"{"ts_ms": 1, "height": 2, "tx_count": 3, "future_field": "x"}"#);
        assert_eq!(
            p,
            ParsedLine::Block(TraceEvent { ts_ms: 1, height: 2, tx_count: Some(3) })
        );
    }

    #[test]
    fn fixture_excerpt_parses_per_spec() {
        // Raw verbatim excerpt of the banked trace: 201 block events,
        // headerless (pre-spec exemption), 40 nulls, 9 height jumps
        // (jumps appear as successive heights -- no consumer-side
        // expansion). See trace-format.md section 8.2.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/feeder/fixtures/trace_excerpt.jsonl"
        );
        let data = std::fs::read(path).expect("fixture must exist");
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(data), sh.clone());
        let events = drain(&mut src);

        assert_eq!(events.len(), 201, "fixture has 201 block events");
        assert!(sh.fatal_message().is_none(), "fixture is monotonic");
        assert_eq!(sh.gaps_skipped.load(Ordering::Relaxed), 0);
        assert_eq!(sh.malformed_lines.load(Ordering::Relaxed), 0);

        let nulls = events.iter().filter(|e| e.tx_count.is_none()).count();
        assert_eq!(nulls, 40, "fixture has 40 null tx_counts");

        assert_eq!(events.first().unwrap().height, 260_138_266);
        assert_eq!(events.last().unwrap().height, 260_138_493);

        // Strictly increasing heights; count the documented 9 jumps.
        let mut jumps = 0;
        for w in events.windows(2) {
            assert!(w[1].height > w[0].height);
            assert!(w[1].ts_ms >= w[0].ts_ms);
            if w[1].height - w[0].height > 1 {
                jumps += 1;
            }
        }
        assert_eq!(jumps, 9, "fixture has 9 height jumps");
    }

    #[test]
    fn header_and_gaps_skipped_and_counted() {
        let trace = concat!(
            r#"{"provenance": {"generator": "record", "params": {}, "generated_at": "2026-06-11T00:00:00Z"}}"#, "\n",
            r#"{"ts_ms": 10, "height": 100, "tx_count": 7}"#, "\n",
            r#"{"gap": true, "ts_ms": 11, "reason": "ws_disconnect"}"#, "\n",
            "this is not json\n",
            r#"{"ts_ms": 12, "height": 105, "tx_count": null}"#, "\n",
        );
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(trace.as_bytes().to_vec()), sh.clone());
        let events = drain(&mut src);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].tx_count, Some(7));
        assert_eq!(events[1].tx_count, None);
        assert_eq!(sh.gaps_skipped.load(Ordering::Relaxed), 1);
        assert_eq!(sh.malformed_lines.load(Ordering::Relaxed), 1);
        assert!(sh.fatal_message().is_none());
        // Gap followed by a height discontinuity (100 -> 105): legal,
        // not expanded (spec 6.2 exception) -- just two events.
    }

    #[test]
    fn height_regression_is_fatal() {
        let trace = concat!(
            r#"{"ts_ms": 10, "height": 100, "tx_count": 1}"#, "\n",
            r#"{"ts_ms": 11, "height": 99, "tx_count": 1}"#, "\n",
            r#"{"ts_ms": 12, "height": 101, "tx_count": 1}"#, "\n",
        );
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(trace.as_bytes().to_vec()), sh.clone());
        let events = drain(&mut src);
        assert_eq!(events.len(), 1, "stream stops at the violation");
        assert!(sh.fatal_message().unwrap().contains("strictly increasing"));
        assert!(sh.shutdown_requested());
    }

    #[test]
    fn duplicate_height_is_fatal() {
        let trace = concat!(
            r#"{"ts_ms": 10, "height": 100, "tx_count": 1}"#, "\n",
            r#"{"ts_ms": 11, "height": 100, "tx_count": 1}"#, "\n",
        );
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(trace.as_bytes().to_vec()), sh.clone());
        assert_eq!(drain(&mut src).len(), 1);
        assert!(sh.fatal_message().is_some());
    }

    #[test]
    fn ts_regression_is_fatal() {
        let trace = concat!(
            r#"{"ts_ms": 10, "height": 100, "tx_count": 1}"#, "\n",
            r#"{"ts_ms": 9, "height": 101, "tx_count": 1}"#, "\n",
        );
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(trace.as_bytes().to_vec()), sh.clone());
        assert_eq!(drain(&mut src).len(), 1);
        assert!(sh.fatal_message().unwrap().contains("ts_ms regressed"));
    }

    #[test]
    fn equal_ts_is_legal() {
        // Height-jump expansion (P2) emits multiple events at the same
        // ts_ms; consumers must accept it.
        let trace = concat!(
            r#"{"ts_ms": 10, "height": 100, "tx_count": 1}"#, "\n",
            r#"{"ts_ms": 10, "height": 101, "tx_count": 1}"#, "\n",
        );
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(trace.as_bytes().to_vec()), sh.clone());
        assert_eq!(drain(&mut src).len(), 2);
        assert!(sh.fatal_message().is_none());
    }

    #[test]
    fn eof_is_clean_shutdown() {
        let sh = shared();
        let mut src = LineSource::new(Cursor::new(Vec::new()), sh.clone());
        assert!(src.next_event().is_none());
        assert!(sh.fatal_message().is_none());
    }
}
