//! The band: one cell for every posting found, oldest at the left.
//! Currents drift across it, open postings glow, and the selected job is the target.

use std::rc::Rc;
use std::time::Instant;

use gpui_kit::{
    App, BorderStyle, Bounds, Font, FontWeight, Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString,
    Styled as _, TextAlign, TextRun, Window, canvas, fill, font, outline, point, px, size,
};
use maverick_core::apply;
use maverick_core::jobs::{Posting, Stage, posting_address};

use crate::jobs::Job;
use crate::ui::{MONO, Palette, Signal};

/// What a posting is to the funnel, which decides how its cell is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// Skipped, a duplicate, or never scored.
    Passed,
    /// Scored and still in play, but not something Maverick can fill.
    Scored(u8),
    /// On the jobs list.
    Open,
    Applied,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub kind: Kind,
    /// The posting's own address, kept for open postings so a job can find its cell.
    pub address: Option<String>,
}

/// One cell for each posting, oldest first. `jobs` are the postings on the jobs list.
pub fn cells(postings: &[Posting], jobs: &[Job]) -> Vec<Cell> {
    let mut oldest_first: Vec<&Posting> = postings.iter().collect();
    oldest_first.sort_by_key(|posting| posting.found_at);
    // Two postings can share an address. Only the first one stands for the job.
    let mut unplaced: Vec<&str> = jobs.iter().map(|job| job.address.as_str()).collect();
    oldest_first
        .into_iter()
        .map(|posting| {
            let in_play = matches!(posting.stage, Stage::Found | Stage::Review);
            let address = apply::posting_address(&posting_address(&posting.link));
            let listed = address
                .as_deref()
                .filter(|_| in_play)
                .and_then(|address| unplaced.iter().position(|open| *open == address));
            if let Some(ix) = listed {
                unplaced.swap_remove(ix);
                return Cell {
                    kind: Kind::Open,
                    address,
                };
            }
            let kind = match (&posting.stage, posting.score) {
                (Stage::Applied, _) => Kind::Applied,
                (Stage::Found | Stage::Review, Some(score)) => Kind::Scored(score),
                _ => Kind::Passed,
            };
            Cell { kind, address: None }
        })
        .collect()
}

/// A job the band points at.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub address: String,
    pub score: Option<u8>,
    pub title: String,
}

pub struct Band {
    cells: Rc<Vec<Cell>>,
    started: Instant,
}

impl Band {
    pub fn new(cells: Vec<Cell>) -> Self {
        Self {
            cells: Rc::new(cells),
            started: Instant::now(),
        }
    }

    /// Turns a job's cell into an applied one, once its application went through.
    pub fn mark_applied(&mut self, address: &str) {
        let Some(ix) = self
            .cells
            .iter()
            .position(|cell| cell.address.as_deref() == Some(address))
        else {
            return;
        };
        Rc::make_mut(&mut self.cells)[ix] = Cell {
            kind: Kind::Applied,
            address: None,
        };
    }

    /// The band as it looks now. `target` is the selected job and `tracked` are the next best.
    pub fn element(&self, target: Option<Mark>, tracked: Vec<Mark>, cx: &App) -> impl IntoElement + use<> {
        let scene = Scene {
            cells: self.cells.clone(),
            target,
            tracked,
            time: self.started.elapsed().as_secs_f32(),
            palette: Palette::of(cx),
        };
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| scene.paint(bounds, window, cx),
        )
        .size_full()
    }
}

/// Where the cells sit inside the band.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Grid {
    /// The distance from one cell to the next. A cell is one pixel smaller.
    pitch: f32,
    columns: usize,
    rows: usize,
}

impl Grid {
    const SMALLEST: usize = 5;
    const LARGEST: usize = 9;
    /// Space kept clear above and below the cells.
    const MARGIN: f32 = 3.0;

    /// The largest cells that still fit `count` postings. When even the smallest do not fit them all,
    /// the grid holds as many as it can.
    fn fit(width: f32, height: f32, count: usize) -> Self {
        let at = |pitch: usize| {
            let pitch = pitch as f32;
            Self {
                pitch,
                columns: (width / pitch).floor().max(0.0) as usize,
                rows: ((height - 2.0 * Self::MARGIN) / pitch).floor().max(0.0) as usize,
            }
        };
        (Self::SMALLEST..=Self::LARGEST)
            .rev()
            .map(at)
            .find(|grid| grid.capacity() >= count)
            .unwrap_or_else(|| at(Self::SMALLEST))
    }

    fn capacity(&self) -> usize {
        self.columns * self.rows
    }

    /// The slot of each of `count` cells, oldest first. The cells cover every column, so time runs
    /// the full width of the band. A column that is not full leaves its gaps at scattered rows.
    fn spread(&self, count: usize) -> Vec<usize> {
        let count = count.min(self.capacity());
        let mut slots = Vec::with_capacity(count);
        let mut rows: Vec<usize> = Vec::with_capacity(self.rows);
        for column in 0..self.columns {
            let held = (column + 1) * count / self.columns - column * count / self.columns;
            rows.clear();
            rows.extend(0..self.rows);
            rows.sort_unstable_by_key(|row| scatter(column, *row));
            rows.truncate(held);
            rows.sort_unstable();
            slots.extend(rows.iter().map(|row| column * self.rows + row));
        }
        slots
    }

    /// The top left corner of a slot, relative to the band. Slots fill a column, then the next one.
    fn corner(&self, slot: usize) -> (f32, f32) {
        let (column, row) = (slot / self.rows.max(1), slot % self.rows.max(1));
        (column as f32 * self.pitch, Self::MARGIN + row as f32 * self.pitch)
    }
}

/// A fixed, uneven number for a slot, so the gaps in a column do not line up with the next column's.
fn scatter(column: usize, row: usize) -> u32 {
    let mut mixed = (column as u32).wrapping_mul(0x9E37_79B1) ^ (row as u32).wrapping_mul(0x85EB_CA6B);
    mixed ^= mixed >> 15;
    mixed = mixed.wrapping_mul(0x2C1B_3C6D);
    mixed ^ (mixed >> 13)
}

/// How strong the current is at a cell, from 0 to 1. Most of the band is still water.
fn current(column: f32, row: f32, time: f32) -> f32 {
    let wave = 0.5
        + 0.25 * (column * 0.11 + time * 0.35 + 1.3 * (row * 0.19 - time * 0.22).sin()).sin()
        + 0.25 * (row * 0.27 - time * 0.3 + 1.7 * (column * 0.06 + time * 0.18).sin()).sin();
    let rise = ((wave - 0.52) / (0.9 - 0.52)).clamp(0.0, 1.0);
    rise * rise * (3.0 - 2.0 * rise)
}

/// The signal a cell is drawn in, or `None` for ink, and how opaque. `None` overall leaves the cell dark.
fn shade(kind: Kind, current: f32, slot: usize, time: f32) -> Option<(Option<Signal>, f32)> {
    const STILL: f32 = 0.04;
    match kind {
        Kind::Open => Some((Some(Signal::Yours), 0.7 + 0.3 * (time * 2.0 + slot as f32 * 0.7).sin())),
        Kind::Applied => Some((Some(Signal::Done), 0.85)),
        Kind::Scored(score) if current < STILL => (score >= 70).then_some((None, 0.07)),
        Kind::Scored(score) => Some((
            None,
            (0.05 + current * 0.9) * (0.45 + 0.55 * f32::from(score.min(100)) / 100.0),
        )),
        Kind::Passed if current < STILL => None,
        Kind::Passed => Some((None, 0.03 + current * 0.16)),
    }
}

struct Scene {
    cells: Rc<Vec<Cell>>,
    target: Option<Mark>,
    tracked: Vec<Mark>,
    time: f32,
    palette: Palette,
}

impl Scene {
    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let grid = Grid::fit(width, height, self.cells.len());
        if grid.capacity() == 0 {
            return;
        }
        // When the band cannot hold every posting, the oldest ones are left out.
        let shown = &self.cells[self.cells.len().saturating_sub(grid.capacity())..];
        let origin = bounds.origin;
        let at = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));
        let cell = px(grid.pitch - 1.0);

        let slots = grid.spread(shown.len());

        for (held, &slot) in shown.iter().zip(&slots) {
            let (x, y) = grid.corner(slot);
            let flow = current(x / grid.pitch, y / grid.pitch, self.time);
            if let Some((signal, opacity)) = shade(held.kind, flow, slot, self.time) {
                let color = signal.map_or(self.palette.ink, |signal| self.palette.signal(signal));
                window.paint_quad(fill(
                    Bounds::new(at(x, y), size(cell, cell)),
                    color.opacity(opacity.clamp(0.0, 1.0)),
                ));
            }
        }

        let center_of = |mark: &Mark| {
            let index = shown
                .iter()
                .position(|held| held.address.as_deref() == Some(mark.address.as_str()))?;
            let (x, y) = grid.corner(slots[index]);
            let half = (grid.pitch - 1.0) / 2.0;
            Some((x + half, y + half))
        };
        let target = self.target.as_ref().and_then(|mark| Some((mark, center_of(mark)?)));
        let tracked: Vec<(&Mark, (f32, f32))> = self
            .tracked
            .iter()
            .filter_map(|mark| Some((mark, center_of(mark)?)))
            .collect();
        let yours = self.palette.signal(Signal::Yours);
        let half = (grid.pitch - 1.0) / 2.0;

        if let Some((_, (target_x, target_y))) = target {
            for (_, (x, y)) in &tracked {
                let mut line = PathBuilder::stroke(px(1.0));
                line.move_to(at(target_x, target_y));
                line.line_to(at(*x, *y));
                if let Ok(line) = line.build() {
                    window.paint_path(line, self.palette.ink.opacity(0.16));
                }
            }
        }
        for (mark, (x, y)) in &tracked {
            window.paint_quad(fill(Bounds::new(at(x - half, y - half), size(cell, cell)), yours));
            let reach = half + 3.0;
            window.paint_quad(outline(
                Bounds::new(at(x - reach, y - reach), size(px(reach * 2.0), px(reach * 2.0))),
                self.palette.ink.opacity(0.7),
                BorderStyle::Solid,
            ));
            if let Some(score) = mark.score {
                self.label(
                    &score.to_string(),
                    (x + reach + 3.0, y - 6.5),
                    width,
                    Plate::Plain,
                    self.palette.muted,
                    origin,
                    window,
                    cx,
                );
            }
        }
        if let Some((mark, (x, y))) = target {
            window.paint_quad(fill(Bounds::new(at(x - half, y - half), size(cell, cell)), yours));
            let reach = half + 7.0;
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let (corner_x, corner_y) = (x + dx * reach, y + dy * reach);
                let arm = 5.0;
                let across = Bounds::new(
                    at(corner_x.min(corner_x - dx * arm), corner_y - 0.5),
                    size(px(arm), px(1.0)),
                );
                let down = Bounds::new(
                    at(corner_x - 0.5, corner_y.min(corner_y - dy * arm)),
                    size(px(1.0), px(arm)),
                );
                window.paint_quad(fill(across, self.palette.ink));
                window.paint_quad(fill(down, self.palette.ink));
            }
            let named = match mark.score {
                Some(score) => format!("TARGET {score}"),
                None => "TARGET".to_owned(),
            };
            let beside = (x + reach + 6.0, y - reach - 1.0);
            self.label(
                &named,
                beside,
                width,
                Plate::Boxed(self.palette.ink),
                self.palette.ink,
                origin,
                window,
                cx,
            );
            let title: String = mark.title.to_uppercase().chars().take(34).collect();
            self.label(
                &title,
                (beside.0, beside.1 + 16.0),
                width,
                Plate::Plain,
                self.palette.muted,
                origin,
                window,
                cx,
            );
        }

        let open = self.cells.iter().filter(|held| held.kind == Kind::Open).count();
        let applied = self.cells.iter().filter(|held| held.kind == Kind::Applied).count();
        self.label(
            &format!("TRACKING {open} OPEN"),
            (14.0, 10.0),
            width,
            Plate::Boxed(self.palette.line),
            self.palette.muted,
            origin,
            window,
            cx,
        );
        let totals = format!("{applied} APPLIED / {} FOUND", grouped(self.cells.len()));
        self.label(
            &totals,
            (width, 10.0),
            width - 14.0,
            Plate::Boxed(self.palette.line),
            self.palette.muted,
            origin,
            window,
            cx,
        );
    }

    /// Draws a small label on its own plate, so it stays readable over the cells.
    /// A label that would cross `right_edge` is moved left until it fits.
    #[expect(clippy::too_many_arguments)]
    fn label(
        &self,
        text: &str,
        (x, y): (f32, f32),
        right_edge: f32,
        plate: Plate,
        color: Hsla,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let run = TextRun {
            len: text.len(),
            font: Font {
                weight: FontWeight::MEDIUM,
                ..font(MONO)
            },
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(SharedString::from(text.to_owned()), px(9.5), &[run], None);
        let (pad, tall) = (5.0, 14.0);
        let wide = f32::from(line.width) + 2.0 * pad;
        let x = x.min(right_edge - wide).max(0.0).round();
        let y = y.round();
        let plate_bounds = Bounds::new(point(origin.x + px(x), origin.y + px(y)), size(px(wide), px(tall)));
        window.paint_quad(fill(plate_bounds, self.palette.background));
        if let Plate::Boxed(border) = plate {
            window.paint_quad(outline(plate_bounds, border, BorderStyle::Solid));
        }
        line.paint(
            point(origin.x + px(x + pad), origin.y + px(y)),
            px(tall),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
    }
}

enum Plate {
    Plain,
    Boxed(Hsla),
}

/// A count with a comma between thousands.
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (ix, digit) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posting(link: &str, found_at: &str, stage: Stage, score: Option<u8>) -> Posting {
        Posting {
            link: link.into(),
            found_at: found_at.parse().unwrap(),
            source: "Notify".into(),
            company: "Example Co".into(),
            title: "Backend Engineer".into(),
            location: None,
            email_subject: None,
            stage,
            score,
            variant: None,
            flags: None,
            notes: None,
        }
    }

    #[test]
    fn each_posting_gets_one_cell_oldest_first_and_a_job_gets_one_open_cell() {
        let ashby = "https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb9455";
        let postings = [
            posting(
                "https://boards.example.com/3",
                "2026-11-03T00:00:00Z",
                Stage::Review,
                Some(71),
            ),
            posting(ashby, "2026-11-02T00:00:00Z", Stage::Found, Some(91)),
            posting(
                "https://boards.example.com/1",
                "2026-11-01T00:00:00Z",
                Stage::Skipped,
                Some(20),
            ),
            posting(
                &format!("{ashby}?utm_source=x"),
                "2026-11-04T00:00:00Z",
                Stage::Review,
                Some(91),
            ),
            posting(
                "https://boards.example.com/5",
                "2026-11-05T00:00:00Z",
                Stage::Applied,
                Some(88),
            ),
            posting(
                "https://boards.example.com/6",
                "2026-11-06T00:00:00Z",
                Stage::Found,
                None,
            ),
        ];
        let jobs = [Job {
            company: "Example Co".into(),
            title: "Backend Engineer".into(),
            score: Some(91),
            address: ashby.into(),
            ..Job::default()
        }];

        let mut band = Band::new(cells(&postings, &jobs));
        let kinds = |band: &Band| band.cells.iter().map(|cell| cell.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds(&band),
            [
                Kind::Passed,
                Kind::Open,
                Kind::Scored(71),
                Kind::Scored(91),
                Kind::Applied,
                Kind::Passed
            ]
        );
        assert_eq!(band.cells[1].address.as_deref(), Some(ashby));

        band.mark_applied(ashby);
        assert_eq!(kinds(&band)[1], Kind::Applied);
    }

    #[test]
    fn the_grid_uses_the_largest_cells_that_fit_every_posting() {
        assert_eq!(
            Grid::fit(1080.0, 164.0, 2160),
            Grid {
                pitch: 8.0,
                columns: 135,
                rows: 19
            }
        );
        assert_eq!(Grid::fit(1080.0, 164.0, 12).pitch, 9.0);
        // More postings than the smallest cells can hold.
        let full = Grid::fit(300.0, 60.0, 5000);
        assert_eq!((full.pitch, full.capacity()), (5.0, 60 * 10));
        assert_eq!(full.corner(0), (0.0, 3.0));
        assert_eq!(full.corner(10), (5.0, 3.0));
    }

    #[test]
    fn cells_cover_every_column_in_order_and_a_full_grid_has_no_gaps() {
        let grid = Grid::fit(2550.0, 164.0, 2160);
        let slots = grid.spread(2160);
        assert_eq!(slots.len(), 2160);
        assert!(slots.windows(2).all(|pair| pair[0] < pair[1]));
        let per_column = |column: usize| slots.iter().filter(|slot| *slot / grid.rows == column).count();
        let fair = 2160 / grid.columns;
        assert!((0..grid.columns).all(|column| (fair..=fair + 1).contains(&per_column(column))));
        // The gaps do not sit at the same rows in every column.
        let rows_of = |column: usize| -> Vec<usize> {
            slots
                .iter()
                .filter(|slot| *slot / grid.rows == column)
                .map(|slot| slot % grid.rows)
                .collect()
        };
        assert_ne!(rows_of(0), rows_of(1));

        let full = Grid::fit(300.0, 60.0, 5000);
        assert_eq!(full.spread(5000), (0..full.capacity()).collect::<Vec<_>>());
        assert_eq!(full.spread(0), Vec::<usize>::new());
    }

    #[test]
    fn open_and_applied_cells_always_show_and_passed_cells_show_only_in_a_current() {
        assert!(
            shade(Kind::Open, 0.0, 3, 1.0)
                .is_some_and(|(signal, opacity)| signal == Some(Signal::Yours) && opacity >= 0.4)
        );
        assert_eq!(shade(Kind::Applied, 0.0, 3, 1.0), Some((Some(Signal::Done), 0.85)));
        assert_eq!(shade(Kind::Passed, 0.0, 3, 1.0), None);
        assert!(shade(Kind::Passed, 1.0, 3, 1.0).is_some_and(|(signal, opacity)| signal.is_none() && opacity < 0.25));
        let (low, high) = (
            shade(Kind::Scored(40), 1.0, 3, 1.0).unwrap().1,
            shade(Kind::Scored(95), 1.0, 3, 1.0).unwrap().1,
        );
        assert!(high > low);
        assert_eq!(grouped(2160), "2,160");
        assert_eq!(grouped(42), "42");
    }
}
