use crate::{
    Config, Error, InputFormat,
    converter::{FILTER_LEN, FLUSH_BLOCKS, INPUT_FRAMES, MAX_RAW_SAMPLES},
};

/// Conservative retained-audio capacity, in target mono PCM frames (not allocation bytes).
///
/// Every lane rounds upward separately. Duplicate representations count separately; empty pool
/// capacity and zero padding do not imply actual audio, but counting their capacity is conservative.
/// The OS-owned callback slice is borrowed only for the callback; this budget covers library/host
/// retention, not driver/device buffers that CPAL does not expose or bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioBudget {
    /// All raw pool buffers, including callback and converter-owned in-flight blocks.
    pub raw: u64,
    /// Mono staging, the resampler's input/history buffer, and a separate filter-delay allowance.
    pub converter_input: u64,
    /// Floating output scratch, per-push PCM vector, and worst-case flush PCM vector.
    pub converter_output: u64,
    /// Native outbound queue and one copied pending send chunk.
    pub native_output: u64,
    /// Host queue, host worker pending chunk, and consumer-owned frame credits.
    pub external: u64,
    /// Checked sum of all simultaneous conservative capacities.
    pub total: u64,
}
fn add(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_add(b).ok_or(Error::Limit)
}
fn mul(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_mul(b).ok_or(Error::Limit)
}
fn ceil(n: u64, d: u64) -> Result<u64, Error> {
    Ok(add(n, d - 1)? / d)
}

impl Config {
    /// Calculate the actual-format capacity before allocating raw blocks or opening a stream.
    ///
    /// This includes the whole capture path even before RPC admission. RPC audio input is empty
    /// until Accepted; after admission its separately bounded queue belongs to the RPC contract.
    /// `external_chunks` must include every host queue slot, pending send, and consumer frame.
    ///
    /// # Errors
    /// Rejects invalid config/layout/credit counts or checked-arithmetic overflow.
    pub fn audio_budget(
        &self,
        input: InputFormat,
        external_chunks: usize,
    ) -> Result<AudioBudget, Error> {
        let slots = self.callback_queue_items(input)?;
        self.audio_budget_for_slots(input, external_chunks, slots)
    }

    /// Select the largest requested callback reserve fitting the actual-format one-second budget.
    /// Without aggregate credits the configured raw queue bound is used unchanged.
    ///
    /// # Errors
    /// Rejects invalid formats/configuration. If even the minimum reserve exceeds the budget,
    /// returns one slot so the budget remains inspectable; preflight rejects that layout.
    pub fn callback_queue_items(&self, input: InputFormat) -> Result<usize, Error> {
        self.validate()?;
        for slots in (1..=self.limits.raw_queue_items).rev() {
            let external = self.limits.preaccept_external_chunks.unwrap_or(0);
            let budget = self.audio_budget_for_slots(input, external, slots)?;
            if self.limits.preaccept_external_chunks.is_none()
                || budget.total <= u64::from(self.target_rate)
            {
                return Ok(slots);
            }
        }
        Ok(1)
    }

    fn audio_budget_for_slots(
        &self,
        input: InputFormat,
        external_chunks: usize,
        slots: usize,
    ) -> Result<AudioBudget, Error> {
        self.validate()?;
        if !(8_000..=192_000).contains(&input.rate) || !(1..=32).contains(&input.channels) {
            return Err(Error::UnsupportedFormat);
        }
        if external_chunks > 64 {
            return Err(Error::InvalidConfig);
        }
        let target = u64::from(self.target_rate);
        let rate = u64::from(input.rate);
        let raw_frames = MAX_RAW_SAMPLES as u64 / u64::from(input.channels);
        let to_target = |frames| ceil(mul(frames, target)?, rate);
        // There are slots+1 buffers TOTAL, not slots queued plus slots+1 filled pool buffers.
        let raw = to_target(mul(raw_frames, slots as u64 + 1)?)?;
        // rubato 0.16.2 SincFixedIn retains chunk+2*filter input frames internally.
        // Count staging and filter delay again, rather than depending on overlap or skipped output.
        let converter_input = to_target((2 * INPUT_FRAMES + 3 * FILTER_LEN) as u64)?;
        // rubato output_frames_max: floor(chunk*ratio + 10). Ceil is a conservative integer bound.
        let output_block = add(to_target(INPUT_FRAMES as u64)?, 10)?;
        let push_blocks = add(ceil(raw_frames, INPUT_FRAMES as u64)?, 1)?;
        // Flush needs at most staging+filter history; four full blocks exceed that tail.
        // Sum push and flush vectors even though the production owner never retains both together.
        let converter_output = mul(output_block, add(push_blocks, FLUSH_BLOCKS as u64 + 1)?)?;
        let (slots, chunk) = self.output_layout();
        let native_output = mul(slots as u64 + 1, chunk as u64 / 2)?;
        let external = mul(
            external_chunks as u64,
            self.limits.max_chunk_bytes as u64 / 2,
        )?;
        let total = [
            raw,
            converter_input,
            converter_output,
            native_output,
            external,
        ]
        .into_iter()
        .try_fold(0, add)?;
        Ok(AudioBudget {
            raw,
            converter_input,
            converter_output,
            native_output,
            external,
            total,
        })
    }

    pub(crate) fn preflight(&self, input: InputFormat) -> Result<(), Error> {
        if let Some(external) = self.limits.preaccept_external_chunks
            && self.audio_budget(input, external)?.total > u64::from(self.target_rate)
        {
            return Err(Error::PreacceptBudgetExceeded);
        }
        Ok(())
    }

    pub(crate) fn output_layout(&self) -> (usize, usize) {
        let slots = self
            .limits
            .output_queue_items
            .min(self.limits.output_queue_bytes / 2);
        let chunk = self
            .limits
            .max_chunk_bytes
            .min(self.limits.output_queue_bytes / slots / 2 * 2);
        (slots, chunk)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn checked_arithmetic_rejects_overflow() {
        assert_eq!(super::mul(u64::MAX, 2), Err(crate::Error::Limit));
        assert_eq!(super::ceil(u64::MAX, 2), Err(crate::Error::Limit));
    }
}
