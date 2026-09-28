//! Default colours for the 269-slot terminal colour table.
//!
//! A terminal's colour table has three layers. [`Palette`] is the bottom one: the theme's
//! defaults. On top sit runtime overrides set by the program via OSC 4/10/11, which
//! `Term` records in [`alacritty_terminal::term::color::Colors`]. Use [`Palette::resolve`]
//! to combine the two — it is the single place that decides what colour an index means,
//! so the renderer and the OSC-query reply can never disagree.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{NamedColor, Rgb};

/// Number of slots in the colour table, mirroring `term::color::COUNT`.
pub const COUNT: usize = alacritty_terminal::term::color::COUNT;

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Theme colours: the 16 ANSI colours plus the special foreground/background/cursor.
///
/// Indices 16..256 are not stored — the 6×6×6 cube and the grey ramp are defined by the
/// xterm standard and computed on demand, and no theme should be redefining them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The 8 normal colours followed by the 8 bright ones.
    pub ansi: [Rgb; 16],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
}

impl Default for Palette {
    /// A dark default in the same family as the usual terminal themes.
    fn default() -> Self {
        Self {
            ansi: [
                rgb(0x1d, 0x1f, 0x21), // black
                rgb(0xcc, 0x66, 0x66), // red
                rgb(0xb5, 0xbd, 0x68), // green
                rgb(0xf0, 0xc6, 0x74), // yellow
                rgb(0x81, 0xa2, 0xbe), // blue
                rgb(0xb2, 0x94, 0xbb), // magenta
                rgb(0x8a, 0xbe, 0xb7), // cyan
                rgb(0xc5, 0xc8, 0xc6), // white
                rgb(0x66, 0x66, 0x66), // bright black
                rgb(0xd5, 0x4e, 0x53), // bright red
                rgb(0xb9, 0xca, 0x4a), // bright green
                rgb(0xe7, 0xc5, 0x47), // bright yellow
                rgb(0x7a, 0xa6, 0xda), // bright blue
                rgb(0xc3, 0x97, 0xd8), // bright magenta
                rgb(0x70, 0xc0, 0xb1), // bright cyan
                rgb(0xea, 0xea, 0xea), // bright white
            ],
            foreground: rgb(0xc5, 0xc8, 0xc6),
            background: rgb(0x1d, 0x1f, 0x21),
            cursor: rgb(0xc5, 0xc8, 0xc6),
        }
    }
}

impl Palette {
    /// The theme's colour for `index`, ignoring any runtime override.
    ///
    /// Out-of-range indices yield the foreground rather than panicking: the value comes
    /// from escape sequences under the remote program's control, and a malformed one
    /// should discolour a cell, not take down the terminal.
    pub fn default_color(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.ansi[index],
            // 6×6×6 colour cube.
            16..=231 => {
                let i = index - 16;
                let level = |v: usize| -> u8 {
                    // xterm's non-linear ramp: 0, 95, 135, 175, 215, 255.
                    if v == 0 {
                        0
                    } else {
                        (v * 40 + 55) as u8
                    }
                };
                rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            // 24-step grey ramp from #080808 to #eeeeee.
            232..=255 => {
                let v = (8 + (index - 232) * 10) as u8;
                rgb(v, v, v)
            }
            i if i == NamedColor::Foreground as usize => self.foreground,
            i if i == NamedColor::Background as usize => self.background,
            i if i == NamedColor::Cursor as usize => self.cursor,
            // Dim variants of the 8 normal colours.
            i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                dim(self.ansi[i - NamedColor::DimBlack as usize])
            }
            i if i == NamedColor::BrightForeground as usize => self.ansi[15],
            i if i == NamedColor::DimForeground as usize => dim(self.foreground),
            _ => self.foreground,
        }
    }

    /// The effective colour for `index`: a runtime override if the program set one,
    /// otherwise the theme default.
    pub fn resolve(&self, colors: &Colors, index: usize) -> Rgb {
        if index < COUNT {
            if let Some(rgb) = colors[index] {
                return rgb;
            }
        }
        self.default_color(index)
    }
}

/// Two-thirds intensity, the conventional rendering of the SGR "dim" attribute.
fn dim(c: Rgb) -> Rgb {
    rgb(
        (c.r as u16 * 2 / 3) as u8,
        (c.g as u16 * 2 / 3) as u8,
        (c.b as u16 * 2 / 3) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_matches_xterm_ramp() {
        // The corners of the 6x6x6 cube are the values xterm defines.
        assert_eq!(Palette::default().default_color(16), rgb(0, 0, 0));
        assert_eq!(Palette::default().default_color(231), rgb(255, 255, 255));
        // Index 21 is (0, 0, 5) -> pure blue at full ramp.
        assert_eq!(Palette::default().default_color(21), rgb(0, 0, 255));
        // Index 46 is (0, 5, 0) -> pure green.
        assert_eq!(Palette::default().default_color(46), rgb(0, 255, 0));
        // The second ramp step is 95, not 51: the ramp is deliberately non-linear.
        assert_eq!(Palette::default().default_color(17), rgb(0, 0, 95));
    }

    #[test]
    fn grey_ramp_endpoints() {
        assert_eq!(Palette::default().default_color(232), rgb(8, 8, 8));
        assert_eq!(Palette::default().default_color(255), rgb(238, 238, 238));
    }

    #[test]
    fn named_slots_map_to_theme() {
        let p = Palette::default();
        assert_eq!(
            p.default_color(NamedColor::Foreground as usize),
            p.foreground
        );
        assert_eq!(
            p.default_color(NamedColor::Background as usize),
            p.background
        );
        assert_eq!(p.default_color(NamedColor::Cursor as usize), p.cursor);
        assert_eq!(p.default_color(NamedColor::BrightRed as usize), p.ansi[9]);
        assert_eq!(p.default_color(NamedColor::DimRed as usize), dim(p.ansi[1]));
    }

    #[test]
    fn out_of_range_index_does_not_panic() {
        let p = Palette::default();
        assert_eq!(p.default_color(usize::MAX), p.foreground);
    }

    #[test]
    fn runtime_override_wins_over_theme() {
        let p = Palette::default();
        let mut colors = Colors::default();
        assert_eq!(p.resolve(&colors, 1), p.ansi[1]);

        colors[1] = Some(rgb(1, 2, 3));
        assert_eq!(p.resolve(&colors, 1), rgb(1, 2, 3));

        // Clearing the override falls back to the theme.
        colors[1] = None;
        assert_eq!(p.resolve(&colors, 1), p.ansi[1]);
    }
}
