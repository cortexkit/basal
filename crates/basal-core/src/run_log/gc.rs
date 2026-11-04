use super::*;
use std::collections::BTreeSet;

pub(super) struct Contents {
    pub end: SegmentEnd,
    pub runs: Option<BTreeSet<String>>,
}

impl Contents {
    pub fn new(end: SegmentEnd) -> Self {
        Self {
            end,
            runs: Some(BTreeSet::new()),
        }
    }

    pub fn note(&mut self, run_id: Option<&str>) {
        match (self.runs.as_mut(), run_id) {
            (Some(runs), Some(id)) => {
                runs.insert(id.to_owned());
            }
            (_, None) => self.runs = None,
            _ => {}
        }
    }
}

impl Engine {
    pub(super) fn reclaim(&mut self, live_starts: &BTreeMap<String, Lsn>) -> Result<usize> {
        self.check()?;
        let candidates: Vec<_> = self
            .contents
            .values()
            .take_while(|s| s.end.lsn.sequence < self.header.seq && s.runs.is_some())
            .map(|s| ReclaimSegment {
                end: s.end,
                runs: s.runs.clone().unwrap(),
            })
            .collect();
        let Some(deleted) = self.registry.reclaim(
            self.config.lease_epoch,
            &candidates,
            self.synced,
            live_starts,
        )?
        else {
            return Ok(0);
        };
        self.at(Boundary::RunLogGcCommitted)?;
        let sequences: Vec<_> = self
            .contents
            .range(..=deleted.lsn.sequence)
            .map(|(&seq, _)| seq)
            .collect();
        for &seq in &sequences {
            self.check()?;
            std::fs::remove_file(segment_path(&self.directory, seq))?;
            self.at(Boundary::RunLogGcUnlinked)?;
            self.contents.remove(&seq);
        }
        self.check()?;
        platform::sync_dir(&self.directory)?;
        self.at(Boundary::RunLogGcSynced)?;
        Ok(sequences.len())
    }
}
