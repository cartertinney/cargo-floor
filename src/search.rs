use crate::progress::Progress;
use anyhow::Result;
use serde::Serialize;
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Binary,
    Exhaustive,
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Exhaustive => "exhaustive",
        }
    }
}

pub struct SearchResult {
    pub floor: Option<usize>,
    pub earlier_candidates_checked: bool,
}

pub fn find_floor(
    count: usize,
    anchor: Option<usize>,
    strategy: Strategy,
    progress: &Progress,
    probe: impl FnMut(usize) -> Result<bool>,
) -> Result<SearchResult> {
    let mut probes = Probes {
        outcomes: vec![None; count],
        run: probe,
    };
    let mut floor = match strategy {
        Strategy::Binary if count > 0 => probes.binary(anchor, progress)?,
        Strategy::Binary => None,
        Strategy::Exhaustive => probes.first_pass(0..count)?,
    };
    if let Some(best) = floor {
        if !probes.earlier_checked(best) && probes.non_monotonic() {
            progress.message(format_args!(
                "Observed non-monotonic results; checking earlier candidates exhaustively"
            ));
            floor = probes.first_pass(0..best + 1)?;
        }
    }
    Ok(SearchResult {
        floor,
        earlier_candidates_checked: floor.is_some_and(|best| probes.earlier_checked(best)),
    })
}

struct Probes<F> {
    outcomes: Vec<Option<bool>>,
    run: F,
}

impl<F: FnMut(usize) -> Result<bool>> Probes<F> {
    fn test(&mut self, candidate: usize) -> Result<bool> {
        if let Some(outcome) = self.outcomes[candidate] {
            return Ok(outcome);
        }
        let outcome = (self.run)(candidate)?;
        self.outcomes[candidate] = Some(outcome);
        Ok(outcome)
    }

    fn first_pass(&mut self, range: Range<usize>) -> Result<Option<usize>> {
        for candidate in range {
            if self.test(candidate)? {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    fn binary(&mut self, anchor: Option<usize>, progress: &Progress) -> Result<Option<usize>> {
        if self.test(0)? {
            return Ok(Some(0));
        }
        let count = self.outcomes.len();
        if count == 1 {
            return Ok(None);
        }
        let mut upper = None;
        for candidate in anchor.into_iter().chain(std::iter::once(count - 1)) {
            if self.test(candidate)? {
                upper = Some(candidate);
                break;
            }
        }
        let Some(mut upper) = upper else {
            progress.message(format_args!(
                "No passing binary-search endpoint; falling back to exhaustive search"
            ));
            return self.first_pass(0..count);
        };
        let mut lower = 0;
        while upper - lower > 1 {
            let middle = lower + (upper - lower) / 2;
            if self.test(middle)? {
                upper = middle;
            } else {
                lower = middle;
            }
        }
        let mut best = upper;
        for neighbor in upper.saturating_sub(2)..upper {
            if self.test(neighbor)? {
                best = best.min(neighbor);
            }
        }
        Ok(Some(best))
    }

    fn earlier_checked(&self, candidate: usize) -> bool {
        self.outcomes[..candidate]
            .iter()
            .all(|outcome| *outcome == Some(false))
    }

    fn non_monotonic(&self) -> bool {
        let mut passed = false;
        for outcome in self.outcomes.iter().flatten() {
            if *outcome {
                passed = true;
            } else if passed {
                return true;
            }
        }
        false
    }
}
