//! Draws the GB350 boot logo (shown by the NOR bootloader from
//! `cubegm/xgame-logo.bmp` until Linux takes the display) in the launcher's
//! style, as a 640x480 32-bit top-down BMP. The header is byte for byte the
//! one of a BMP the bootloader is known to accept (the stock logo, odd size
//! fields included), so only the pixels differ.
//!
//!   cargo run -p gb350-ui --example bootlogo -- <out.bmp> [preview.ppm]

use gb350_ui::style::{ACCENT, BG, DIM};
use gb350_ui::{Canvas, Face, MaskBuilder, Ui};

const W: i32 = 640;
const H: i32 = 480;

/// BITMAPFILEHEADER + BITMAPINFOHEADER: 640 x -480 (top-down), 32 bits.
const HEADER: [u8; 54] = [
    0x42, 0x4d, 0x48, 0xc0, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00,
    0x80, 0x02, 0x00, 0x00, 0x20, 0xfe, 0xff, 0xff, 0x01, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xc0,
    0x12, 0x00, 0x12, 0x0b, 0x00, 0x00, 0x12, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

fn draw(c: &mut Canvas) {
    c.fill(BG);
    // Mark: a yellow rounded square with a D-pad, then the name.
    let (mark, gap) = (88, 22);
    let name = "GB350";
    let name_w = c.ui.measure(Face::DisplayBold, 76, name, 2);
    let x0 = (W - (mark + gap + name_w)) / 2;
    let y0 = H / 2 - mark / 2 - 12;
    c.round_rect(x0, y0, mark, mark, 20, ACCENT);
    c.icon("dpad", x0 + 18, y0 + 18, BG, || {
        let (s, t) = (52.0, 18.0);
        let o = (s - t) / 2.0;
        MaskBuilder::new(52, 52).rrect(o, 0.0, t, s, 4.0).rrect(0.0, o, s, t, 4.0).build()
    });
    // The pad's centre dimple, so it reads as a D-pad rather than a plus.
    c.disc(x0 + mark / 2 - 5, y0 + mark / 2 - 5, 10, 0x34373D);
    let b = c.ui.baseline_center(Face::DisplayBold, 76, y0 + mark / 2);
    c.text_ls(Face::DisplayBold, 76, x0 + mark + gap, b, 0xEDEDED, name, 2);
    // A thin accent rule under it, and a quiet line of text.
    let rule_y = y0 + mark + 34;
    c.rect(W / 2 - 24, rule_y, 48, 3, ACCENT);
    c.text_mid_center(Face::Sans, 14, W / 2, rule_y + 30, DIM, "starting up");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: bootlogo <out.bmp> [preview.ppm]");
        std::process::exit(2);
    };

    let mut ui = Ui::new();
    let mut px = vec![0u32; (W * H) as usize];
    draw(&mut Canvas::new(&mut px, W, H, W as usize, &mut ui));

    let mut bmp = HEADER.to_vec();
    for p in &px {
        // BGRA, opaque.
        bmp.extend_from_slice(&[*p as u8, (p >> 8) as u8, (p >> 16) as u8, 0xff]);
    }
    std::fs::write(out, &bmp).expect("write bmp");
    if let Some(preview) = args.get(1) {
        let mut ppm = format!("P6\n{W} {H}\n255\n").into_bytes();
        for p in &px {
            ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
        std::fs::write(preview, ppm).expect("write preview");
    }
    println!("{out}: {} bytes", bmp.len());
}
