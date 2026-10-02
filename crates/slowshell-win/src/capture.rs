//! Reading a window's pixels back.
//!
//! Two uses, both about not guessing. A shell can be "blank" for a dozen
//! different reasons, and the only way to tell them apart is to look at what
//! actually reached the screen. The render probe uses this to assert that a fill
//! landed where it was asked to; a screenshot feature would use the same call.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use slowshell_core::Color;

/// A 24-bit RGB image, top-down.
pub struct Image {
    data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Image {
    /// The colour at a pixel, or transparent if the point is out of bounds.
    ///
    /// `GetDIBits` fills a 24-bit buffer in **BGR** order, so the channels are
    /// swapped here rather than leaving every caller to remember.
    pub fn pixel(&self, x: u32, y: u32) -> Color {
        if x >= self.width || y >= self.height {
            return Color::TRANSPARENT;
        }
        let i = ((y * self.width + x) * 3) as usize;
        Color::rgb(self.data[i + 2], self.data[i + 1], self.data[i])
    }

    /// How many pixels in a box are darker than `threshold` (0..1).
    ///
    /// Text is the awkward thing to assert on: no single pixel decides whether a
    /// string was drawn, but a count of dark pixels does.
    pub fn dark_pixels_in(&self, x: u32, y: u32, w: u32, h: u32, threshold: f32) -> usize {
        let mut n = 0;
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                if self.pixel(xx, yy).luminance() < threshold {
                    n += 1;
                }
            }
        }
        n
    }

    /// Whether two colours are close enough to be the same after compositing and
    /// colour conversion.
    pub fn matches(got: Color, want: Color, tolerance: i32) -> bool {
        (got.r as i32 - want.r as i32).abs() <= tolerance
            && (got.g as i32 - want.g as i32).abs() <= tolerance
            && (got.b as i32 - want.b as i32).abs() <= tolerance
    }
}

/// Copy a window's whole client area into an image.
pub fn grab_client(hwnd: HWND) -> Option<Image> {
    grab(hwnd, 0, 0, 0, 0)
}

/// Copy a window's client area, or a rectangle of it, into an image.
///
/// A zero width or height means "ask the window how big it is". Goes through
/// GDI, which reads the same surface the user is looking at — so this sees what
/// was presented, not what was drawn into a back buffer.
pub fn grab(hwnd: HWND, x: i32, y: i32, width: u32, height: u32) -> Option<Image> {
    unsafe {
        let window_dc = GetDC(Some(hwnd));
        if window_dc.is_invalid() {
            return None;
        }
        let (w, h) = if width == 0 || height == 0 {
            let mut r = RECT::default();
            if GetClientRect(hwnd, &mut r).is_err() {
                let _ = ReleaseDC(Some(hwnd), window_dc);
                return None;
            }
            (
                if width == 0 { (r.right - r.left).max(1) as u32 } else { width },
                if height == 0 { (r.bottom - r.top).max(1) as u32 } else { height },
            )
        } else {
            (width, height)
        };

        let mem = CreateCompatibleDC(Some(window_dc));
        if mem.is_invalid() {
            let _ = ReleaseDC(Some(hwnd), window_dc);
            return None;
        }
        let bmp = CreateCompatibleBitmap(window_dc, w as i32, h as i32);
        if bmp.is_invalid() {
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(Some(hwnd), window_dc);
            return None;
        }
        let previous = SelectObject(mem, bmp.into());
        let copied = BitBlt(mem, 0, 0, w as i32, h as i32, Some(window_dc), x, y, SRCCOPY).is_ok();

        let mut data = vec![0u8; (w * h * 3) as usize];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                // A negative height asks for a top-down image, which saves a flip.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 24,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            // A one-entry colour table is the documented minimum even with
            // `biClrUsed` set to zero.
            bmiColors: [windows::Win32::Graphics::Gdi::RGBQUAD::default(); 1],
        };
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h,
            Some(data.as_mut_ptr() as *mut core::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        );

        // Undo everything in the reverse order it was set up, so a failed probe
        // leaks neither a DC nor a bitmap.
        let _ = SelectObject(mem, previous);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(Some(hwnd), window_dc);

        (copied && lines > 0).then_some(Image { data, width: w, height: h })
    }
}
