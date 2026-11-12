use std::io::{Read, Write};

/// Copy `r` to `w`, replacing every occurrence of `pat` with `rep`.
/// Streams as it goes, holding back only bytes that could start a match.
pub fn pipe(mut r: impl Read, mut w: impl Write, pat: &[u8], rep: &[u8]) {
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 8192];
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) | Err(_) => {
                let _ = w.write_all(&buf);
                let _ = w.flush();
                return;
            }
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while let Some(p) = find(&buf[i..], pat) {
            out.extend_from_slice(&buf[i..i + p]);
            out.extend_from_slice(rep);
            i += p + pat.len();
        }
        let keep = partial_suffix(&buf[i..], pat);
        out.extend_from_slice(&buf[i..buf.len() - keep]);
        buf.drain(..buf.len() - keep);
        if w.write_all(&out).is_err() || w.flush().is_err() {
            return;
        }
    }
}

fn find(hay: &[u8], pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    hay.windows(pat.len()).position(|w| w == pat)
}

/// Length of the longest suffix of `hay` that is a proper prefix of `pat`.
fn partial_suffix(hay: &[u8], pat: &[u8]) -> usize {
    let max = hay.len().min(pat.len().saturating_sub(1));
    (1..=max).rev().find(|&k| hay[hay.len() - k..] == pat[..k]).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Chunks(Vec<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let c = self.0.remove(0);
            out[..c.len()].copy_from_slice(&c);
            Ok(c.len())
        }
    }

    #[test]
    fn replaces_across_chunk_boundaries() {
        let input = Chunks(vec![b"err at /slot/sr".to_vec(), b"c/main.rs and /slot/src/x".to_vec()]);
        let mut out = Vec::new();
        pipe(input, &mut out, b"/slot/src", b"/wt");
        assert_eq!(out, b"err at /wt/main.rs and /wt/x");
    }

    fn run(chunks: &[&[u8]], pat: &[u8], rep: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        pipe(Chunks(chunks.iter().map(|c| c.to_vec()).collect()), &mut out, pat, rep);
        out
    }

    #[test]
    fn passes_through_without_matches() {
        assert_eq!(run(&[b"hello ", b"world"], b"/slot", b"/wt"), b"hello world");
        assert_eq!(run(&[], b"/slot", b"/wt"), b"");
    }

    #[test]
    fn partial_match_at_eof_is_flushed() {
        assert_eq!(run(&[b"ends with /sl"], b"/slot", b"/wt"), b"ends with /sl");
    }

    #[test]
    fn adjacent_and_repeated_matches() {
        assert_eq!(run(&[b"/slot/slot", b"/slot"], b"/slot", b"/wt"), b"/wt/wt/wt");
    }

    #[test]
    fn one_byte_chunks() {
        let input = b"a /slot/src b /slot/src";
        let chunks: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(run(&chunks, b"/slot/src", b"/wt"), b"a /wt b /wt");
    }

    #[test]
    fn false_start_inside_partial_match() {
        // `//slot` contains a false start: the first `/` begins a partial match.
        assert_eq!(run(&[b"x//sl", b"ot"], b"/slot", b"/wt"), b"x//wt");
    }

    #[test]
    fn replacement_may_contain_pattern() {
        assert_eq!(run(&[b"/a/a"], b"/a", b"/a/a"), b"/a/a/a/a");
    }

    #[test]
    fn non_utf8_passes_through() {
        assert_eq!(run(&[b"\xff/slot\xfe"], b"/slot", b"/wt"), b"\xff/wt\xfe");
    }
}
