//! A tiny self-playing Tetris — the ambient animation rendered into the Preferences
//! preview pane. It's a normal locked terminal: each step the controller advances the
//! game and `feed`s a fresh ANSI frame, so it animates like any other terminal output and
//! picks up the drafted font + colour theme for free.
//!
//! Deliberately minimal: a naive 4×4 rotation (no wall kicks), a one-piece "AI" that drops
//! each tetromino toward a random column, line clears, and an auto-reset when it tops out —
//! enough to read as Tetris at a glance, not a playable game.

/// Playfield size (cells). Each cell renders 2 terminal columns wide so blocks look square.
/// A tall, narrow well so the board fills the tall preview column.
const W: i32 = 10;
const H: i32 = 26;

/// The 7 tetrominoes, each as 4 cells in a 4×4 box (spawn rotation). Rotated 90° CW with
/// `(x,y) -> (3 - y, x)`.
const PIECES: [[(i32, i32); 4]; 7] = [
    [(0, 1), (1, 1), (2, 1), (3, 1)], // I
    [(1, 0), (2, 0), (1, 1), (2, 1)], // O
    [(1, 0), (0, 1), (1, 1), (2, 1)], // T
    [(1, 0), (2, 0), (0, 1), (1, 1)], // S
    [(0, 0), (1, 0), (1, 1), (2, 1)], // Z
    [(0, 0), (0, 1), (1, 1), (2, 1)], // J
    [(2, 0), (0, 1), (1, 1), (2, 1)], // L
];

/// Points awarded for clearing 0..=4 lines at once (classic Tetris scoring, ×level).
const LINE_SCORE: [u32; 5] = [0, 100, 300, 500, 800];

pub struct Tetris {
    /// Locked cells: 0 = empty, else `piece_kind + 1` (so the colour survives the lock).
    board: [[u8; W as usize]; H as usize],
    rng: u64,
    kind: usize,
    next: usize,    // the upcoming piece (shown in the HUD's NEXT)
    cells: [(i32, i32); 4], // current piece's rotated cells (within its 4×4 box)
    x: i32,
    y: i32,         // box offset on the board
    target_x: i32,  // column the "AI" is sliding the piece toward
    score: u32,
    lines: u32,
}

impl Tetris {
    pub fn new(seed: u64) -> Self {
        let mut t = Tetris {
            board: [[0; W as usize]; H as usize],
            rng: seed | 1,
            kind: 0,
            next: 0,
            cells: PIECES[0],
            x: 0,
            y: 0,
            target_x: 0,
            score: 0,
            lines: 0,
        };
        t.next = (t.rand() % 7) as usize;
        t.spawn();
        t
    }

    pub fn score(&self) -> u32 {
        self.score
    }
    pub fn lines(&self) -> u32 {
        self.lines
    }
    /// Level rises every 10 cleared lines (1-based), like classic Tetris.
    pub fn level(&self) -> u32 {
        self.lines / 10 + 1
    }
    /// The upcoming piece's kind (for the HUD's NEXT swatch).
    pub fn next_kind(&self) -> usize {
        self.next
    }
    /// The upcoming piece's tetromino letter (I/O/T/S/Z/J/L), for the HUD's NEXT.
    pub fn next_letter(&self) -> char {
        ['I', 'O', 'T', 'S', 'Z', 'J', 'L'][self.next]
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn rotate(cells: &[(i32, i32); 4]) -> [(i32, i32); 4] {
        let mut r = *cells;
        for c in r.iter_mut() {
            *c = (3 - c.1, c.0);
        }
        r
    }

    /// Whether `cells` at box offset `(ox, oy)` would overlap a wall, the floor, or a
    /// locked cell. (Above the top — `y < 0` — is allowed so a piece can spawn off-screen.)
    fn collides(&self, cells: &[(i32, i32); 4], ox: i32, oy: i32) -> bool {
        for (cx, cy) in cells {
            let x = ox + cx;
            let y = oy + cy;
            if x < 0 || x >= W || y >= H {
                return true;
            }
            if y >= 0 && self.board[y as usize][x as usize] != 0 {
                return true;
            }
        }
        false
    }

    fn spawn(&mut self) {
        self.kind = self.next;
        self.next = (self.rand() % 7) as usize;
        // Pick the best landing (rotation + column) so the auto-player actually clears lines.
        let (cells, target) = self.best_placement();
        self.cells = cells;
        self.target_x = target;
        self.x = W / 2 - 2;
        self.y = 0;
        // Topped out → start a fresh game (wipe board + reset score) and keep playing.
        if self.collides(&self.cells, self.x, self.y) {
            self.board = [[0; W as usize]; H as usize];
            self.score = 0;
            self.lines = 0;
        }
    }

    /// The lowest box offset `oy` at which `cells` rests at column `ox` (a hard drop), or
    /// `None` if the piece can't sit in that column at all.
    fn drop_y(&self, cells: &[(i32, i32); 4], ox: i32) -> Option<i32> {
        let mut y = -3;
        if self.collides(cells, ox, y) {
            return None; // can't place this column/rotation
        }
        while !self.collides(cells, ox, y + 1) {
            y += 1;
        }
        Some(y)
    }

    /// The board that would result from locking `cells` at `(ox, oy)`.
    fn with_locked(&self, cells: &[(i32, i32); 4], ox: i32, oy: i32) -> [[u8; W as usize]; H as usize] {
        let mut b = self.board;
        for (cx, cy) in cells {
            let x = ox + cx;
            let y = oy + cy;
            if (0..W).contains(&x) && (0..H).contains(&y) {
                b[y as usize][x as usize] = 1;
            }
        }
        b
    }

    /// Greedy one-piece placement (El-Tetris weights): try every rotation × column, hard-drop
    /// each, and score the resulting board by aggregate height / completed lines / holes /
    /// bumpiness. Returns the chosen rotated cells + target column.
    fn best_placement(&self) -> ([(i32, i32); 4], i32) {
        let mut rot = PIECES[self.kind];
        let mut best_score = f64::MIN;
        let mut best = (rot, W / 2 - 2);
        for _ in 0..4 {
            for ox in -2..=W + 1 {
                if let Some(oy) = self.drop_y(&rot, ox) {
                    let score = Self::evaluate(&self.with_locked(&rot, ox, oy));
                    if score > best_score {
                        best_score = score;
                        best = (rot, ox);
                    }
                }
            }
            rot = Self::rotate(&rot);
        }
        best
    }

    /// Score a board for the placement AI: reward completed lines, punish height/holes/bumps.
    fn evaluate(board: &[[u8; W as usize]; H as usize]) -> f64 {
        let mut heights = [0i32; W as usize];
        for x in 0..W as usize {
            for y in 0..H as usize {
                if board[y][x] != 0 {
                    heights[x] = H - y as i32;
                    break;
                }
            }
        }
        let agg: i32 = heights.iter().sum();
        let mut holes = 0;
        for x in 0..W as usize {
            let mut seen = false;
            for y in 0..H as usize {
                if board[y][x] != 0 {
                    seen = true;
                } else if seen {
                    holes += 1;
                }
            }
        }
        let mut bump = 0;
        for x in 0..W as usize - 1 {
            bump += (heights[x] - heights[x + 1]).abs();
        }
        let lines = (0..H as usize)
            .filter(|&y| (0..W as usize).all(|x| board[y][x] != 0))
            .count() as i32;
        -0.51 * agg as f64 + 0.76 * lines as f64 - 0.36 * holes as f64 - 0.18 * bump as f64
    }

    fn lock(&mut self) {
        let color = (self.kind + 1) as u8;
        for (cx, cy) in self.cells {
            let x = self.x + cx;
            let y = self.y + cy;
            if (0..W).contains(&x) && (0..H).contains(&y) {
                self.board[y as usize][x as usize] = color;
            }
        }
        let cleared = self.clear_lines();
        self.lines += cleared as u32;
        // Score before the level bump (classic: points scale with the level you cleared at).
        self.score += LINE_SCORE[cleared.min(4)] * self.level();
        self.spawn();
    }

    /// Remove full rows, compacting the rest down. Returns how many were cleared.
    fn clear_lines(&mut self) -> usize {
        let mut write = H - 1;
        let mut kept = 0;
        for read in (0..H).rev() {
            let full = (0..W).all(|x| self.board[read as usize][x as usize] != 0);
            if !full {
                if write != read {
                    self.board[write as usize] = self.board[read as usize];
                }
                write -= 1;
                kept += 1;
            }
        }
        for y in 0..=write {
            self.board[y as usize] = [0; W as usize];
        }
        H as usize - kept
    }

    /// Advance one frame: nudge the piece a column toward its target, then apply gravity —
    /// locking (and spawning the next piece) when it can fall no further.
    pub fn step(&mut self) {
        if self.x < self.target_x && !self.collides(&self.cells, self.x + 1, self.y) {
            self.x += 1;
        } else if self.x > self.target_x && !self.collides(&self.cells, self.x - 1, self.y) {
            self.x -= 1;
        }
        if !self.collides(&self.cells, self.x, self.y + 1) {
            self.y += 1;
        } else {
            self.lock();
        }
    }

    /// The board's width in terminal columns (each cell is 2 glyphs wide).
    pub const COLS: usize = (W * 2) as usize;
    /// The board's height in terminal rows.
    pub const ROWS: usize = H as usize;

    /// The board as `ROWS` rows joined by `\r\n` (no leading/trailing control), with the
    /// falling piece overlaid. Filled cells use truecolor `██` from `colors` (the active
    /// frame palette, indexed by piece kind); empty cells are spaces so the terminal theme's
    /// background shows through. The caller frames it with the HUD + cursor-home.
    pub fn render(&self, colors: &[(u8, u8, u8)]) -> String {
        let mut disp = self.board;
        let color = (self.kind + 1) as u8;
        for (cx, cy) in self.cells {
            let x = self.x + cx;
            let y = self.y + cy;
            if (0..W).contains(&x) && (0..H).contains(&y) {
                disp[y as usize][x as usize] = color;
            }
        }

        let mut rows: Vec<String> = Vec::with_capacity(H as usize);
        for row in &disp {
            let mut line = String::with_capacity(Self::COLS * 4);
            for &c in row {
                if c == 0 {
                    line.push_str("  ");
                } else {
                    let (r, g, b) = colors[(c as usize - 1) % colors.len().max(1)];
                    line.push_str(&format!("\x1b[38;2;{};{};{}m██\x1b[0m", r, g, b));
                }
            }
            rows.push(line);
        }
        rows.join("\r\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAL: [(u8, u8, u8); 4] = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0)];

    #[test]
    fn board_has_the_right_shape() {
        let t = Tetris::new(0xC0FFEE);
        let board = t.render(&PAL);
        // ROWS rows, CRLF-joined; COLS / ROWS expose the board's terminal extent.
        let lines: Vec<&str> = board.split("\r\n").collect();
        assert_eq!(lines.len(), Tetris::ROWS);
        assert_eq!(Tetris::COLS, (W * 2) as usize);
    }

    #[test]
    fn it_runs_forever_without_panicking() {
        let mut t = Tetris::new(1);
        // Many steps exercise spawn / slide / lock / line-clear / top-out reset.
        for _ in 0..5000 {
            t.step();
            let _ = t.render(&PAL);
        }
    }
}
