use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};

/// Resolved 8-bit RGB color.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Default foreground / background. A soft off-white on a near-black with a
/// faint cool tint — modern and easy on the eyes (Catppuccin-Mocha inspired).
pub const DEFAULT_FG: Rgba = Rgba::new(0xcd, 0xd6, 0xf4);
pub const DEFAULT_BG: Rgba = Rgba::new(0x11, 0x11, 0x1b);

/// The 16 ANSI base colors (normal 0-7, bright 8-15). Modern, vivid but not
/// neon — distinct normal/bright pairs so colored output reads cleanly.
const ANSI16: [Rgba; 16] = [
    Rgba::new(0x45, 0x47, 0x5a), // black  (surface1)
    Rgba::new(0xf3, 0x8b, 0xa8), // red
    Rgba::new(0xa6, 0xe3, 0xa1), // green
    Rgba::new(0xf9, 0xe2, 0xaf), // yellow
    Rgba::new(0x89, 0xb4, 0xfa), // blue
    Rgba::new(0xcb, 0xa6, 0xf7), // magenta
    Rgba::new(0x94, 0xe2, 0xd5), // cyan
    Rgba::new(0xba, 0xc2, 0xde), // white  (subtext1)
    Rgba::new(0x58, 0x5b, 0x70), // bright black  (surface2)
    Rgba::new(0xf5, 0x9f, 0xbb), // bright red
    Rgba::new(0xb5, 0xe8, 0xb0), // bright green
    Rgba::new(0xfb, 0xe8, 0xc0), // bright yellow
    Rgba::new(0x9c, 0xc1, 0xff), // bright blue
    Rgba::new(0xd5, 0xb6, 0xff), // bright magenta
    Rgba::new(0xa6, 0xed, 0xe1), // bright cyan
    Rgba::new(0xf8, 0xf8, 0xf2), // bright white
];

/// Resolve a 256-color palette index to RGB (the standard xterm cube + ramp).
fn indexed_to_rgb(idx: u8) -> Rgba {
    match idx {
        0..=15 => ANSI16[idx as usize],
        16..=231 => {
            // 6x6x6 color cube.
            let i = idx as u16 - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let step = |v: u16| -> u8 {
                if v == 0 {
                    0
                } else {
                    (55 + v * 40) as u8
                }
            };
            Rgba::new(step(r), step(g), step(b))
        }
        232..=255 => {
            // Grayscale ramp.
            let level = 8 + (idx as u16 - 232) * 10;
            let v = level as u8;
            Rgba::new(v, v, v)
        }
    }
}

fn named_to_rgb(named: NamedColor) -> Rgba {
    use NamedColor::*;
    match named {
        Black => ANSI16[0],
        Red => ANSI16[1],
        Green => ANSI16[2],
        Yellow => ANSI16[3],
        Blue => ANSI16[4],
        Magenta => ANSI16[5],
        Cyan => ANSI16[6],
        White => ANSI16[7],
        BrightBlack => ANSI16[8],
        BrightRed => ANSI16[9],
        BrightGreen => ANSI16[10],
        BrightYellow => ANSI16[11],
        BrightBlue => ANSI16[12],
        BrightMagenta => ANSI16[13],
        BrightCyan => ANSI16[14],
        BrightWhite => ANSI16[15],
        Foreground => DEFAULT_FG,
        Background => DEFAULT_BG,
        BrightForeground => Rgba::new(0xf8, 0xf8, 0xf2),
        DimForeground => Rgba::new(0x93, 0x99, 0xb2),
        // Dim variants: the base color blended toward the background.
        DimBlack => dim(ANSI16[0]),
        DimRed => dim(ANSI16[1]),
        DimGreen => dim(ANSI16[2]),
        DimYellow => dim(ANSI16[3]),
        DimBlue => dim(ANSI16[4]),
        DimMagenta => dim(ANSI16[5]),
        DimCyan => dim(ANSI16[6]),
        DimWhite => dim(ANSI16[7]),
        Cursor => DEFAULT_FG,
    }
}

/// Blend a color ~70% toward itself / 30% darker for "dim" SGR.
const fn dim(c: Rgba) -> Rgba {
    Rgba::new(
        (c.r as u16 * 7 / 10) as u8,
        (c.g as u16 * 7 / 10) as u8,
        (c.b as u16 * 7 / 10) as u8,
    )
}

/// Resolve an alacritty cell color into concrete RGB.
pub fn resolve(color: AnsiColor) -> Rgba {
    match color {
        AnsiColor::Named(n) => named_to_rgb(n),
        AnsiColor::Spec(Rgb { r, g, b }) => Rgba::new(r, g, b),
        AnsiColor::Indexed(i) => indexed_to_rgb(i),
    }
}

/// Resolve a foreground color, promoting the 8 normal ANSI colors to their
/// bright variants when the cell is bold (traditional terminal behavior that
/// makes a lot of colored TUI output look correct).
pub fn resolve_fg(color: AnsiColor, bold: bool) -> Rgba {
    if bold {
        if let AnsiColor::Named(n) = color {
            let idx = n as usize;
            if idx < 8 {
                return ANSI16[idx + 8];
            }
        }
        if let AnsiColor::Indexed(i) = color {
            if i < 8 {
                return ANSI16[i as usize + 8];
            }
        }
    }
    resolve(color)
}

/// Darken an already-resolved color (for the SGR "dim" attribute).
pub const fn dim_rgba(c: Rgba) -> Rgba {
    dim(c)
}
