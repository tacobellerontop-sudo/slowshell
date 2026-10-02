//! A minimal end-to-end check of the renderer.
//!
//! Creates one window, draws a known scene into it, then reads the pixels back
//! and reports what it found. Every rendering bug found while building the shell
//! — text boxes collapsed to nothing, a bar that drew a background and no
//! widgets — produced a blank screen and nothing else. This turns "the screen is
//! empty" into "the pixel at (20, 20) is 255,0,0 as expected", which is the
//! difference between a debugging session and a minute.
//!
//! ```text
//! cargo run -p slowshell --example render_probe
//! ```

use std::time::Duration;

use slowshell_core::Color;
use slowshell_win::capture::{self, Image};
use slowshell_win::render::text::TextStyle;
use slowshell_win::render::{Graphics, Painter, Rect, TextEngine};
use slowshell_win::window::{self, SurfaceRole};

const W: u32 = 400;
const H: u32 = 200;

fn main() {
    slowshell_win::init_process();
    let handle = window::create_surface(SurfaceRole::Popup, 40, 40, W, H, "render probe")
        .expect("could not create a window");
    window::show(handle.hwnd);

    let graphics = Graphics::new().expect("no graphics device");
    graphics.create().expect("could not create the device");
    println!("presentation: {}", graphics.presentation().label());
    let mut surface = graphics
        .create_surface(handle.hwnd, W, H)
        .expect("could not create a surface");
    let mut text = TextEngine::new(graphics.write_factory());

    // What the shell's own labels measure, so a trimmed label can be traced to a
    // measurement rather than guessed at.
    for (label, weight) in [("Apps", 400u16), ("Apps", 600), ("08:37", 400), ("WiFi", 400)] {
        let style = TextStyle { size: 14.0, weight, ..TextStyle::default() };
        println!("measure {label} @{weight}: {:?}", text.measure(label, &style, 96.0));
    }

    // A white page, so a draw call that did not happen is obvious.
    let white = Color::rgb(255, 255, 255);
    let mut ok = true;
    for frame in 0..4 {
        // A window render target presents the *previous* frame, so a few are
        // drawn before anything is checked.
        surface.begin(96.0, Some(white));
        {
            let mut p = Painter::new(surface.target(), &mut text, 96.0);
            p.fill_rect(Rect::new(20.0, 20.0, 100.0, 40.0), Color::rgb(255, 0, 0), 0.0);
            p.fill_rect(Rect::new(140.0, 20.0, 100.0, 40.0), Color::rgb(0, 128, 255), 8.0);
            p.stroke_rect(Rect::new(260.0, 20.0, 100.0, 40.0), Color::rgb(0, 0, 0), 8.0, 2.0);
            p.fill_circle((60.0, 110.0), 20.0, Color::rgb(0, 160, 0));
            let big = TextStyle { size: 28.0, weight: 700, ..TextStyle::default() };
            p.text("Hello", &big, Rect::new(100.0, 85.0, 280.0, 50.0), Color::rgb(0, 0, 0));
            let small = TextStyle { size: 14.0, ..TextStyle::default() };
            p.text("AG", &small, Rect::new(100.0, 140.0, 200.0, 20.0), Color::rgb(0, 0, 0));
            // Every weight the shell offers, because a face DirectWrite will not
            // build renders nothing at all and says nothing about it.
            for (i, weight) in [400u16, 500, 600, 700, 900].iter().enumerate() {
                let s = TextStyle { size: 14.0, weight: *weight, ..TextStyle::default() };
                p.text(
                    &format!("W{weight}"),
                    &s,
                    Rect::new(10.0 + i as f32 * 46.0, 170.0, 44.0, 20.0),
                    Color::rgb(0, 0, 0),
                );
            }
            println!("frame {frame}: {:?}", p.stats());
        }
        surface.present();
        std::thread::sleep(Duration::from_millis(40));
    }

    let Some(shot) = capture::grab_client(handle.hwnd) else {
        println!("could not read the window back");
        window::destroy(handle);
        std::process::exit(2);
    };

    let checks: [(&str, u32, u32, Color); 6] = [
        ("page", 5, 5, white),
        ("red fill", 70, 40, Color::rgb(255, 0, 0)),
        ("blue rounded fill", 190, 40, Color::rgb(0, 128, 255)),
        ("green circle", 60, 110, Color::rgb(0, 160, 0)),
        ("outside the circle", 95, 110, white),
        ("page below the text", 5, 190, white),
    ];
    for (name, x, y, want) in checks {
        let got = shot.pixel(x, y);
        let good = Image::matches(got, want, 12);
        ok &= good;
        println!(
            "{:>22} at ({x:>3},{y:>3}): {} got {} want {}",
            name,
            if good { "ok  " } else { "FAIL" },
            got,
            want
        );
    }

    // Text is the awkward one: no single pixel decides it, so count the dark ones
    // inside each text box. A box that measured as zero height clips the glyphs
    // away and this count drops to nothing.
    let big = shot.dark_pixels_in(100, 85, 280, 50, 0.5);
    println!("dark pixels in the 28pt box: {big} (expected more than 40)");
    ok &= big > 40;
    let small = shot.dark_pixels_in(100, 140, 200, 20, 0.5);
    println!("dark pixels in the 14pt box: {small} (expected more than 10)");
    ok &= small > 10;

    for (i, weight) in [400u16, 500, 600, 700, 900].iter().enumerate() {
        let n = shot.dark_pixels_in(10 + i as u32 * 46, 170, 44, 20, 0.5);
        println!("weight {weight}: {n} dark pixels");
        ok &= n > 3;
    }

    window::destroy(handle);
    println!("\n{}", if ok { "renderer probe: PASS" } else { "renderer probe: FAIL" });
    if !ok {
        std::process::exit(1);
    }
}
