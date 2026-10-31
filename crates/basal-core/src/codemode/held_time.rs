//! Person time requires every in-flight call to be held for a person.

use std::collections::BTreeSet;

pub(super) struct Reason {
    pub reason: Option<String>,
    /// The start of the uninterrupted hold, retained across reason changes.
    pub since_ms: i64,
}

#[derive(Default)]
pub(super) struct PersonTime {
    slices: Vec<Slice>,
}

struct Slice {
    from: i64,
    to: i64,
    flights: BTreeSet<u64>,
    people: BTreeSet<u64>,
}

impl PersonTime {
    pub fn tick(&mut self, from: i64, to: i64, flights: BTreeSet<u64>, people: BTreeSet<u64>) {
        if from >= to {
            return;
        }
        if let Some(last) = self.slices.last_mut()
            && last.to == from
            && last.flights == flights
            && last.people == people
        {
            last.to = to;
        } else {
            self.slices.push(Slice {
                from,
                to,
                flights,
                people,
            });
        }
    }

    // Keep even empty slices while the program is blocked without a sleep:
    // a late reason update can classify elapsed wall time as person time.
    // Retaining the in-flight set lets a late running update turn the entire
    // interval back into wall time, even if another call still needs a person.
    pub fn revise(&mut self, position: u64, since: i64, person: bool) {
        if let Some(index) = self
            .slices
            .iter()
            .position(|s| s.from < since && since < s.to)
        {
            let slice = &mut self.slices[index];
            let tail = Slice {
                from: since,
                to: slice.to,
                flights: slice.flights.clone(),
                people: slice.people.clone(),
            };
            slice.to = since;
            self.slices.insert(index + 1, tail);
        }
        for slice in &mut self.slices {
            if slice.from >= since {
                if person {
                    slice.people.insert(position);
                } else {
                    slice.people.remove(&position);
                }
            }
        }
    }

    pub fn total(&self) -> i64 {
        self.slices
            .iter()
            .filter(|s| !s.flights.is_empty() && s.flights.is_subset(&s.people))
            .fold(0i64, |total, s| {
                total.saturating_add(s.to.saturating_sub(s.from))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_late_updates_require_every_flight_to_remain_a_person_hold() {
        let mut time = PersonTime::default();
        time.tick(0, 10, BTreeSet::from([0]), BTreeSet::from([0]));
        time.tick(10, 50, BTreeSet::from([0, 1]), BTreeSet::from([0, 1]));
        time.revise(0, 20, false);
        assert_eq!(time.total(), 20);
        time.tick(50, 60, BTreeSet::from([0, 1]), BTreeSet::from([1]));
        time.revise(1, 15, false);
        assert_eq!(time.total(), 15);
        time.revise(1, 15, false);
        assert_eq!(time.total(), 15);
    }

    #[test]
    fn late_person_update_only_charges_eligible_intervals() {
        let mut time = PersonTime::default();
        time.tick(10, 20, BTreeSet::from([0]), BTreeSet::new());
        time.tick(30, 40, BTreeSet::from([0]), BTreeSet::new());
        time.revise(0, 15, true);
        assert_eq!(time.total(), 15);
        time.revise(0, 35, false);
        assert_eq!(time.total(), 10);
    }

    #[test]
    fn no_in_flight_calls_cannot_count_as_person_time() {
        let mut time = PersonTime::default();
        time.tick(0, 10, BTreeSet::new(), BTreeSet::new());
        time.revise(0, 0, true);
        assert_eq!(time.total(), 0);
    }
}
