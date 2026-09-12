//! Pure-Rust frame-size adaptation between this project's fixed host frame
//! size (480 samples @ 48kHz, see `dsp_core::FRAME_SAMPLES`) and whatever
//! frame length a backend (e.g. the Weya NC C API's
//! `weya_nc_get_frame_length`) actually wants to be called with.
//!
//! We cannot assume the two ever match: the real `weya_nc.h` API returns a
//! frame length that depends on the sample rate passed to
//! `weya_nc_session_create`, and that value can only be read back at
//! runtime from a loaded session. This adapter makes no assumption about
//! either size and instead buffers/re-chunks in both directions, so
//! `HushBvcStage::process` can hand it host-sized frames unconditionally.
//!
//! Contract with callers: `process_in_place` is called once per host frame
//! with a slice of exactly `host_len` samples. It is filled in place with
//! whatever output samples are available; while the very first `target_len`
//! samples of input have not yet accumulated (start-of-stream latency), the
//! output is silence (`0.0`), never uninitialized or stale data.
//!
//! No heap allocation happens inside `process_in_place` in steady state:
//! all buffers are sized once, up front, in `new`.

use std::collections::VecDeque;

pub struct FrameAdapter {
    host_len: usize,
    target_len: usize,
    input: VecDeque<f32>,
    output: VecDeque<f32>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
}

impl FrameAdapter {
    /// `host_len`: size of the frames this adapter will be called with via
    /// `process_in_place` (always `dsp_core::FRAME_SAMPLES` in production).
    /// `target_len`: size of the frames the wrapped backend function
    /// expects (e.g. the value returned by `weya_nc_get_frame_length`).
    ///
    /// Panics if either length is zero: a zero-length frame is a
    /// configuration error, not a runtime condition to recover from.
    pub fn new(host_len: usize, target_len: usize) -> Self {
        assert!(host_len > 0, "host_len must be non-zero");
        assert!(target_len > 0, "target_len must be non-zero");
        // Generous capacity margin so steady-state operation never grows
        // the deques past their initial allocation (see module docs).
        let cap = 4 * host_len.max(target_len) + 8;
        Self {
            host_len,
            target_len,
            input: VecDeque::with_capacity(cap),
            output: VecDeque::with_capacity(cap),
            scratch_in: vec![0.0; target_len],
            scratch_out: vec![0.0; target_len],
        }
    }

    /// Feed one host-sized frame in, get one host-sized frame of available
    /// output back, in place. `process_fn` is called zero or more times per
    /// invocation (however many full `target_len` chunks are ready), each
    /// time with a full `target_len` input slice and a `target_len` output
    /// slice to fill.
    pub fn process_in_place(
        &mut self,
        frame: &mut [f32],
        process_fn: &mut dyn FnMut(&[f32], &mut [f32]),
    ) {
        debug_assert_eq!(frame.len(), self.host_len, "frame length mismatch");

        self.input.extend(frame.iter().copied());

        while self.input.len() >= self.target_len {
            for slot in self.scratch_in.iter_mut() {
                *slot = self
                    .input
                    .pop_front()
                    .expect("loop condition guarantees enough samples");
            }
            process_fn(&self.scratch_in, &mut self.scratch_out);
            self.output.extend(self.scratch_out.iter().copied());
        }

        for sample in frame.iter_mut() {
            *sample = self.output.pop_front().unwrap_or(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// process_fn used throughout: adds a distinguishing marker so we can
    /// trace exactly which samples ended up where.
    fn marker_process(offset: f32) -> impl FnMut(&[f32], &mut [f32]) {
        move |input, output| {
            for (o, i) in output.iter_mut().zip(input.iter()) {
                *o = i + offset;
            }
        }
    }

    #[test]
    fn equal_sizes_is_immediate_passthrough_with_no_latency() {
        // host_len == target_len: every call processes exactly what came
        // in, no buffering delay at all.
        let mut adapter = FrameAdapter::new(4, 4);
        let mut process = marker_process(100.0);

        let mut frame = [1.0, 2.0, 3.0, 4.0];
        adapter.process_in_place(&mut frame, &mut process);
        assert_eq!(frame, [101.0, 102.0, 103.0, 104.0]);

        let mut frame2 = [5.0, 6.0, 7.0, 8.0];
        adapter.process_in_place(&mut frame2, &mut process);
        assert_eq!(frame2, [105.0, 106.0, 107.0, 108.0]);
    }

    #[test]
    fn target_larger_than_host_introduces_startup_silence_then_correct_data() {
        // host_len=2, target_len=4: needs two host frames' worth of input
        // before the backend can run once, so the first host frame out is
        // silence.
        let mut adapter = FrameAdapter::new(2, 4);
        let mut process = marker_process(100.0);

        // Call 1: input buffer now has [1,2] (only 2 of 4 needed) -> no
        // processing yet -> output is silence.
        let mut f1 = [1.0, 2.0];
        adapter.process_in_place(&mut f1, &mut process);
        assert_eq!(f1, [0.0, 0.0]);

        // Call 2: input buffer now has [1,2,3,4] -> exactly one target
        // chunk -> processed to [101,102,103,104], first 2 drained now.
        let mut f2 = [3.0, 4.0];
        adapter.process_in_place(&mut f2, &mut process);
        assert_eq!(f2, [101.0, 102.0]);

        // Call 3: no new full chunk ready yet (only 2 new samples in),
        // but the tail of the previous processed chunk is still queued.
        let mut f3 = [5.0, 6.0];
        adapter.process_in_place(&mut f3, &mut process);
        assert_eq!(f3, [103.0, 104.0]);

        // Call 4: [5,6,7,8] now complete -> processed -> drained.
        let mut f4 = [7.0, 8.0];
        adapter.process_in_place(&mut f4, &mut process);
        assert_eq!(f4, [105.0, 106.0]);
    }

    #[test]
    fn target_smaller_than_host_processes_multiple_chunks_per_call_no_latency() {
        // host_len=4, target_len=2: each host call spans exactly two
        // target-sized chunks, fully drained within the same call.
        let mut adapter = FrameAdapter::new(4, 2);
        let mut process = marker_process(1000.0);

        let mut frame = [1.0, 2.0, 3.0, 4.0];
        adapter.process_in_place(&mut frame, &mut process);
        assert_eq!(frame, [1001.0, 1002.0, 1003.0, 1004.0]);

        let mut frame2 = [10.0, 20.0, 30.0, 40.0];
        adapter.process_in_place(&mut frame2, &mut process);
        assert_eq!(frame2, [1010.0, 1020.0, 1030.0, 1040.0]);
    }

    #[test]
    fn non_multiple_sizes_still_conserve_every_sample_in_order() {
        // host_len=3, target_len=5: sizes don't divide evenly, so drains
        // can under-run mid-stream (padding a single 0.0 in with otherwise
        // real data) rather than only ever at the very start. Traced by
        // hand below; this pins down the exact expected byte-for-byte
        // behavior of that non-trivial case rather than a loose invariant.
        let mut adapter = FrameAdapter::new(3, 5);
        let mut process = marker_process(0.0); // identity, easiest to trace

        let inputs: [[f32; 3]; 6] = [
            [1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0],
            [7.0, 8.0, 9.0],
            [10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0],
            [16.0, 17.0, 18.0],
        ];

        let mut collected = Vec::new();
        for chunk in inputs.iter() {
            let mut frame = *chunk;
            adapter.process_in_place(&mut frame, &mut process);
            collected.extend_from_slice(&frame);
        }

        // Hand-traced expected output:
        //   call1: input=[1,2,3]              len3<5  -> no process -> out [0,0,0]
        //   call2: input=[..,4,5,6]=6 samples  ->process [1..5]      -> out [1,2,3], buffered [4,5]
        //   call3: input=[..,7,8,9]=4 samples  no process            -> out [4,5,0] (underrun pad)
        //   call4: input=[..,10,11,12]=7       ->process [6..10]     -> out [6,7,8], buffered [9,10]
        //   call5: input=[..,13,14,15]=5       ->process [11..15]    -> out [9,10,11], buffered [12,13,14,15]
        //   call6: input=[16,17,18] len3<5     no process            -> out [12,13,14], buffered [15]
        let expected = [
            0.0, 0.0, 0.0, // call1
            1.0, 2.0, 3.0, // call2
            4.0, 5.0, 0.0, // call3 (underrun mid-stream, not just at startup)
            6.0, 7.0, 8.0, // call4
            9.0, 10.0, 11.0, // call5
            12.0, 13.0, 14.0, // call6
        ];
        assert_eq!(collected, expected);

        // General invariant, independent of the hand trace above: strip
        // the padding zeros and every real sample that did come out must
        // still be the correct value in the correct order (no corruption
        // or reordering), since the input was 1.0..=18.0 fed in order and
        // this transform is identity.
        let mut next_expected = 1.0f32;
        for &s in &collected {
            if s == 0.0 {
                continue;
            }
            assert_eq!(s, next_expected);
            next_expected += 1.0;
        }
    }
}
