//! Note edits for the piano roll: each takes a pattern's notes and the selected
//! ones (an empty selection means all), and returns the new notes and the new
//! selection, ready for `Action::SetNotes`.

use crate::Note;

/// Notes and selection after an edit.
pub type Edited = (Vec<Note>, Vec<Note>);

fn picked(selected: &[Note], n: &Note) -> bool {
    selected.is_empty() || selected.contains(n)
}

/// Applies `f` to every picked note; `None` from `f` deletes the note.
pub fn map_notes(notes: &[Note], selected: &[Note], f: impl Fn(Note) -> Option<Note>) -> Edited {
    let (mut out, mut sel) = (Vec::with_capacity(notes.len()), Vec::new());
    for n in notes {
        if picked(selected, n) {
            if let Some(m) = f(*n).filter(Note::is_valid) {
                out.push(m);
                if !selected.is_empty() {
                    sel.push(m);
                }
            }
        } else {
            out.push(*n);
        }
    }
    (out, sel)
}

/// Snaps note starts to the nearest multiple of `grid` ticks (lengths too, to at
/// least one grid step, when `lengths` is set).
pub fn quantize(notes: &[Note], selected: &[Note], grid: u64, lengths: bool) -> Edited {
    let grid = grid.max(1);
    let round = |t: u64| (t + grid / 2) / grid * grid;
    map_notes(notes, selected, |n| {
        Some(Note {
            start: round(n.start),
            length: if lengths {
                round(n.length).max(grid)
            } else {
                n.length
            },
            ..n
        })
    })
}

/// Moves picked notes by `semitones`; does nothing if any would leave 0..=127.
pub fn transpose(notes: &[Note], selected: &[Note], semitones: i32) -> Edited {
    let fits = notes
        .iter()
        .filter(|n| picked(selected, n))
        .all(|n| (0..=127).contains(&(i32::from(n.key) + semitones)));
    if !fits {
        let sel = selected.to_vec();
        return (notes.to_vec(), sel);
    }
    map_notes(notes, selected, |n| {
        Some(Note {
            key: (i32::from(n.key) + semitones) as u8,
            ..n
        })
    })
}

/// Moves picked notes by `ticks` (and `keys`); clamps at the pattern start.
pub fn shift(notes: &[Note], selected: &[Note], ticks: i64, keys: i32) -> Edited {
    let earliest = notes
        .iter()
        .filter(|n| picked(selected, n))
        .map(|n| n.start)
        .min()
        .unwrap_or(0);
    let ticks = ticks.max(-(earliest as i64));
    let (moved, sel) = map_notes(notes, selected, |n| {
        Some(Note {
            start: (n.start as i64 + ticks) as u64,
            ..n
        })
    });
    if keys == 0 {
        return (moved, sel);
    }
    transpose(&moved, &sel, keys)
}

/// A copy of the picked notes right after them, by their span rounded up to
/// `grid`. The copies become the selection.
pub fn duplicate(notes: &[Note], selected: &[Note], grid: u64) -> Edited {
    let grid = grid.max(1);
    let picks: Vec<Note> = notes
        .iter()
        .filter(|n| picked(selected, n))
        .copied()
        .collect();
    let (Some(first), Some(last)) = (
        picks.iter().map(|n| n.start).min(),
        picks.iter().map(Note::end).max(),
    ) else {
        return (notes.to_vec(), Vec::new());
    };
    let span = (last - first).div_ceil(grid) * grid;
    let copies: Vec<Note> = picks
        .iter()
        .map(|n| Note {
            start: n.start + span,
            ..*n
        })
        .collect();
    let mut out = notes.to_vec();
    out.extend(&copies);
    (out, copies)
}

/// Stretches each picked note to the start of the next later note (any key), so
/// lines play without gaps or overlaps. The last note keeps its length.
pub fn legato(notes: &[Note], selected: &[Note]) -> Edited {
    let mut starts: Vec<u64> = notes
        .iter()
        .filter(|n| picked(selected, n))
        .map(|n| n.start)
        .collect();
    starts.sort_unstable();
    starts.dedup();
    map_notes(notes, selected, |n| {
        let next = starts.iter().find(|&&s| s > n.start);
        Some(Note {
            length: next.map_or(n.length, |&s| s - n.start),
            ..n
        })
    })
}

/// Scales picked velocities by `factor`, clamped to 1..=127.
pub fn scale_velocity(notes: &[Note], selected: &[Note], factor: f32) -> Edited {
    map_notes(notes, selected, |n| {
        Some(Note {
            velocity: (f32::from(n.velocity) * factor).round().clamp(1.0, 127.0) as u8,
            ..n
        })
    })
}

/// Nudges picked notes by up to `ticks` in time and `velocity` in velocity, from a
/// repeatable pseudo-random sequence seeded by `seed`.
pub fn humanize(notes: &[Note], selected: &[Note], ticks: u64, velocity: u8, seed: u64) -> Edited {
    let state = std::cell::Cell::new(seed | 1);
    // xorshift64: deterministic, so a humanize can be repeated exactly.
    let next = |range: i64| {
        let mut x = state.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state.set(x);
        if range == 0 {
            0
        } else {
            (x % (2 * range as u64 + 1)) as i64 - range
        }
    };
    map_notes(notes, selected, |n| {
        let start = (n.start as i64 + next(ticks as i64)).max(0) as u64;
        let v = (i64::from(n.velocity) + next(i64::from(velocity))).clamp(1, 127) as u8;
        Some(Note {
            start,
            velocity: v,
            ..n
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PPQ, STEP};

    fn n(start: u64, length: u64, key: u8) -> Note {
        Note {
            start,
            length,
            key,
            velocity: 100,
        }
    }

    #[test]
    fn quantize_snaps_only_the_selection() {
        let notes = [n(10, 50, 60), n(STEP + 130, 300, 62)];
        let (out, sel) = quantize(&notes, &[], STEP, true);
        assert_eq!(out, [n(0, STEP, 60), n(2 * STEP, STEP, 62)]);
        assert!(sel.is_empty());
        let (out, sel) = quantize(&notes, &notes[1..], STEP, false);
        assert_eq!(out, [notes[0], n(2 * STEP, 300, 62)]);
        assert_eq!(sel, [n(2 * STEP, 300, 62)]);
    }

    #[test]
    fn transpose_refuses_to_leave_the_keyboard() {
        let notes = [n(0, 1, 120), n(0, 1, 10)];
        assert_eq!(transpose(&notes, &[], 12).0, notes);
        assert_eq!(
            transpose(&notes, &notes[1..], 12).0,
            [notes[0], n(0, 1, 22)]
        );
        assert_eq!(
            shift(&notes, &notes[1..], -500, -1).0,
            [notes[0], n(0, 1, 9)]
        );
    }

    #[test]
    fn duplicate_legato_velocity_and_humanize() {
        let notes = [n(0, STEP, 60), n(STEP, 2 * STEP, 64)];
        let (out, copies) = duplicate(&notes, &[], PPQ);
        assert_eq!(copies, [n(PPQ, STEP, 60), n(PPQ + STEP, 2 * STEP, 64)]);
        assert_eq!(out.len(), 4);

        let gappy = [n(0, 10, 60), n(STEP * 2, 10, 62), n(STEP * 2, 5, 65)];
        let (out, _) = legato(&gappy, &[]);
        assert_eq!(
            out.iter().map(|x| x.length).collect::<Vec<_>>(),
            [2 * STEP, 10, 5]
        );

        let (out, _) = scale_velocity(&notes, &[], 2.0);
        assert!(out.iter().all(|x| x.velocity == 127));

        let a = humanize(&notes, &[], 20, 10, 42);
        assert_eq!(a, humanize(&notes, &[], 20, 10, 42), "repeatable");
        for (h, o) in a.0.iter().zip(&notes) {
            assert!(h.start.abs_diff(o.start) <= 20);
            assert!(h.velocity.abs_diff(o.velocity) <= 10);
        }
    }
}
