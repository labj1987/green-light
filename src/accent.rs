//! The desktop's accent colour, from the XDG desktop portal.
//!
//! GNOME 47+, KDE Plasma and COSMIC publish the colour the user picked as
//! `org.freedesktop.appearance` `accent-color` on the portal's Settings
//! interface: red, green and blue in 0..=1, with a value outside that range
//! meaning "none chosen". The window paints its accent-carrying colours from it
//! (primary buttons, checked boxes, switches and radios, focus rings, links,
//! selections, the selected row, progress and slider bars) in light and dark
//! alike, and follows changes while it runs.
//!
//! Without an accent (no portal, a desktop that publishes none, an older
//! GNOME) nothing here touches the theme: the toolkit's colours stay as they
//! are. The toolkit's `accent` colour is its neutral hover grey, not a brand
//! colour, so it stays as well, as do the status colours (success, warning,
//! danger, info).

use gpui_kit::component::{Theme, ThemeColor, ThemeMode};
use gpui_kit::{App, Global, Hsla, Rgba};

/// A colour in sRGB, each channel in 0..=1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// What text on a coloured surface needs to be read (WCAG 2 AA, normal text).
const TEXT_CONTRAST: f32 = 4.5;
/// What a control's outline or fill needs against its background to be seen
/// (WCAG 2 non-text contrast).
const UI_CONTRAST: f32 = 3.0;
/// Links: the toolkit draws a hovered link at 80% and a pressed one at 60%
/// opacity, ignoring the theme's own hover and active link colours, so the
/// link itself reads at [`LINK_CONTRAST`] and still at [`TEXT_CONTRAST`] when
/// pressed.
const LINK_CONTRAST: f32 = 7.0;
const LINK_PRESSED_OPACITY: f32 = 0.6;
/// Text on an accent surface when white does not reach [`TEXT_CONTRAST`].
const NEAR_BLACK: Rgb = Rgb { r: 10. / 255., g: 10. / 255., b: 10. / 255. };
const WHITE: Rgb = Rgb { r: 1., g: 1., b: 1. };
const BLACK: Rgb = Rgb { r: 0., g: 0., b: 0. };

impl Rgb {
    /// The portal's `(ddd)` value; `None` when a channel is outside 0..=1 or
    /// not a number, which is how the portal says no accent is set.
    pub fn from_portal(r: f64, g: f64, b: f64) -> Option<Rgb> {
        let ok = |c: f64| c.is_finite() && (0.0..=1.0).contains(&c);
        (ok(r) && ok(g) && ok(b)).then_some(Rgb { r: r as f32, g: g as f32, b: b as f32 })
    }

    pub fn from_hsla(color: Hsla) -> Rgb {
        let Rgba { r, g, b, .. } = color.to_rgb();
        Rgb { r, g, b }
    }

    fn hsla(self, alpha: f32) -> Hsla {
        Rgba { r: self.r, g: self.g, b: self.b, a: alpha }.into()
    }

    /// WCAG 2 relative luminance.
    pub fn luminance(self) -> f32 {
        let lin = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// `amount` of the way from this colour to `other`.
    fn mix(self, other: Rgb, amount: f32) -> Rgb {
        let t = amount.clamp(0., 1.);
        Rgb { r: self.r + (other.r - self.r) * t, g: self.g + (other.g - self.g) * t, b: self.b + (other.b - self.b) * t }
    }
}

/// WCAG 2 contrast ratio, 1..=21.
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (a.luminance(), b.luminance());
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Text for a surface of colour `surface`: white when it reads well enough,
/// else near-black, else black (which always does once white does not).
pub fn text_on(surface: Rgb) -> Rgb {
    [WHITE, NEAR_BLACK].into_iter().find(|&text| contrast(text, surface) >= TEXT_CONTRAST).unwrap_or(BLACK)
}

/// `color`, moved towards black on a light `against` and towards white on a
/// dark one, just far enough to reach `ratio`.
pub fn with_contrast(color: Rgb, against: Rgb, ratio: f32) -> Rgb {
    let target = if against.luminance() > 0.18 { BLACK } else { WHITE };
    (0..=50).map(|step| color.mix(target, step as f32 / 50.)).find(|&c| contrast(c, against) >= ratio).unwrap_or(target)
}

/// The link colour for `accent` on `background`: the accent, darkened or
/// lightened until it reads at [`LINK_CONTRAST`], and at [`TEXT_CONTRAST`]
/// at the opacity the toolkit draws a pressed link with.
pub fn link_on(accent: Rgb, background: Rgb) -> Rgb {
    let target = if background.luminance() > 0.18 { BLACK } else { WHITE };
    let reads = |c: Rgb| contrast(c, background) >= LINK_CONTRAST && contrast(background.mix(c, LINK_PRESSED_OPACITY), background) >= TEXT_CONTRAST;
    (0..=50).map(|step| accent.mix(target, step as f32 / 50.)).find(|&c| reads(c)).unwrap_or(target)
}

/// Hover and active shades (8% and 16% steps) of `fill`, which carries
/// `on_fill` text on a window of `background`. They step away from the
/// window, so they stand out from it as the fill does, when the text still
/// reads on them; else away from the text, so it keeps reading; and in half
/// steps when a full step breaks either.
fn hover_and_active(fill: Rgb, on_fill: Rgb, background: Rgb) -> (Rgb, Rgb) {
    let outward = if background.luminance() > 0.18 { BLACK } else { WHITE };
    let away = if on_fill == WHITE { BLACK } else { WHITE };
    let holds = |c: Rgb| contrast(on_fill, c) >= TEXT_CONTRAST && contrast(c, background) >= UI_CONTRAST;
    [(outward, 1.), (away, 1.), (outward, 0.5), (away, 0.5)]
        .into_iter()
        .map(|(to, scale): (Rgb, f32)| (fill.mix(to, 0.08 * scale), fill.mix(to, 0.16 * scale)))
        .find(|&(hover, active)| holds(hover) && holds(active))
        .unwrap_or((fill.mix(away, 0.08), fill.mix(away, 0.16)))
}

/// The colours the accent sets, for one mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Filled controls: primary buttons, checked boxes, radios and switches.
    /// The accent itself where it stands out from the window (3:1), else
    /// darkened or lightened until it does.
    pub fill: Rgb,
    pub fill_hover: Rgb,
    pub fill_active: Rgb,
    /// Text and icons on `fill`, `fill_hover` and `fill_active`.
    pub on_fill: Rgb,
    /// Outlines and bars on the window background: focus ring, selected row
    /// border, progress and slider bars.
    pub line: Rgb,
    /// Link text on the window background.
    pub link: Rgb,
    pub link_hover: Rgb,
    /// Text selection behind `foreground` text.
    pub selection: Rgb,
}

impl Palette {
    /// The palette for `accent` on a window of `background` with `foreground`
    /// text.
    pub fn new(accent: Rgb, background: Rgb, foreground: Rgb) -> Palette {
        let line = with_contrast(accent, background, UI_CONTRAST);
        let fill = line;
        let on_fill = text_on(fill);
        let (fill_hover, fill_active) = hover_and_active(fill, on_fill, background);
        let link = link_on(accent, background);
        let further = if background.luminance() > 0.18 { BLACK } else { WHITE };
        Palette {
            fill,
            fill_hover,
            fill_active,
            on_fill,
            line,
            link,
            link_hover: link.mix(further, 0.2),
            selection: with_contrast(accent, foreground, TEXT_CONTRAST),
        }
    }
}

/// Paints `accent` onto `colors`, a theme of the given darkness, or, with no
/// accent, puts back what `base` (the mode's own colours) has for every
/// colour the accent sets. Alphas of translucent colours (the selected row,
/// the drop target) are kept from `base`.
pub fn paint(colors: &mut ThemeColor, accent: Option<Rgb>, base: &ThemeColor) {
    let Some(accent) = accent else {
        restore(colors, base);
        return;
    };
    let p = Palette::new(accent, Rgb::from_hsla(base.background), Rgb::from_hsla(base.foreground));
    let (fill, hover, active, on_fill) = (p.fill.hsla(1.), p.fill_hover.hsla(1.), p.fill_active.hsla(1.), p.on_fill.hsla(1.));
    colors.primary = fill;
    colors.primary_hover = hover;
    colors.primary_active = active;
    colors.primary_foreground = on_fill;
    colors.button_primary = fill;
    colors.button_primary_hover = hover;
    colors.button_primary_active = active;
    colors.button_primary_foreground = on_fill;
    colors.sidebar_primary = fill;
    colors.sidebar_primary_foreground = on_fill;
    let line = p.line.hsla(1.);
    colors.ring = line;
    colors.list_active_border = line;
    colors.table_active_border = line;
    colors.drag_border = line;
    colors.progress_bar = line;
    colors.slider_bar = line;
    colors.link = p.link.hsla(1.);
    colors.link_active = p.link.hsla(1.);
    colors.link_hover = p.link_hover.hsla(1.);
    colors.selection = p.selection.hsla(base.selection.a);
    colors.list_active = accent.hsla(base.list_active.a);
    colors.table_active = accent.hsla(base.table_active.a);
    colors.drop_target = accent.hsla(base.drop_target.a);
    // A switch's thumb sits on the fill when on: the theme's own thumb where it stands out (3:1), else the
    // fill's text colour.
    colors.switch_thumb = if contrast(Rgb::from_hsla(base.switch_thumb), p.fill) < UI_CONTRAST { on_fill } else { base.switch_thumb };
}

/// Every colour [`paint`] sets, from `base`.
fn restore(colors: &mut ThemeColor, base: &ThemeColor) {
    macro_rules! copy {
        ($($field:ident),+) => { $(colors.$field = base.$field;)+ };
    }
    copy!(
        primary,
        primary_hover,
        primary_active,
        primary_foreground,
        button_primary,
        button_primary_hover,
        button_primary_active,
        button_primary_foreground,
        sidebar_primary,
        sidebar_primary_foreground,
        ring,
        list_active_border,
        table_active_border,
        drag_border,
        progress_bar,
        slider_bar,
        link,
        link_active,
        link_hover,
        selection,
        list_active,
        table_active,
        drop_target,
        switch_thumb
    );
}

/// The accent the portal last gave; absent until it has given one.
struct DesktopAccent(Option<Rgb>);

impl Global for DesktopAccent {}

/// The theme's own colours for `mode`, before anything was painted on them.
fn base_colors(mode: ThemeMode, cx: &App) -> ThemeColor {
    let current = Theme::global(cx);
    let config = if mode.is_dark() { current.dark_theme.clone() } else { current.light_theme.clone() };
    let mut theme = Theme::default();
    theme.apply_config(&config);
    theme.colors
}

/// Paints the desktop's accent onto the theme. Call it after every change of
/// light or dark mode, which reloads the mode's own colours. Does nothing
/// until the portal has given an accent.
pub fn apply(cx: &mut App) {
    let Some(&DesktopAccent(accent)) = cx.try_global::<DesktopAccent>() else { return };
    let base = base_colors(Theme::global(cx).mode, cx);
    Theme::update(cx, |theme| paint(&mut theme.colors, accent, &base));
}

/// Whether a reading of `accent` calls for a repaint, given the reading
/// before it (`None`: none yet). A first reading of no accent does not: the
/// theme has its own colours already.
fn repaints(before: Option<Option<Rgb>>, accent: Option<Rgb>) -> bool {
    match before {
        None => accent.is_some(),
        Some(before) => before != accent,
    }
}

/// Takes `accent` as the desktop's and repaints when it changed.
fn set(accent: Option<Rgb>, cx: &mut App) {
    if repaints(cx.try_global::<DesktopAccent>().map(|current| current.0), accent) {
        cx.set_global(DesktopAccent(accent));
        apply(cx);
    }
}

/// The portal's bus name.
const PORTAL: &str = "org.freedesktop.portal.Desktop";

/// The accent in a `SettingChanged` value: `Some(None)` for "no accent",
/// `None` when the value has another shape. The portal documents `(ddd)`;
/// COSMIC's sends an array of three doubles in the signal (its `ReadOne`
/// answers `(ddd)`).
fn accent_in(value: &ashpd::zbus::zvariant::Value<'_>) -> Option<Option<Rgb>> {
    use ashpd::zbus::zvariant::Value;
    let channels: Vec<f64> = match value {
        Value::Value(inner) => return accent_in(inner),
        Value::Structure(fields) => fields.fields().iter().map(|c| f64::try_from(c).ok()).collect::<Option<_>>()?,
        Value::Array(items) => items.iter().map(|c| f64::try_from(c).ok()).collect::<Option<_>>()?,
        _ => return None,
    };
    match channels[..] {
        [r, g, b] => Some(Rgb::from_portal(r, g, b)),
        _ => None,
    }
}

/// Reads the accent from the portal and follows its changes, reading it again
/// whenever the portal (re)starts: at login it may come up after the window,
/// and it can be restarted. Without a portal or an accent the theme is left as
/// it is.
pub fn follow(cx: &mut App) {
    cx.spawn(async move |cx| watch(cx).await).detach();
}

/// Whether a `NameOwnerChanged` for the portal says it is now running.
fn portal_started(new_owner: Option<&str>) -> bool {
    new_owner.is_some_and(|owner| !owner.is_empty())
}

async fn watch(cx: &mut gpui_kit::AsyncApp) -> Option<()> {
    use ashpd::desktop::settings::Settings;
    use ashpd::zbus;
    use futures_util::future::{select, Either};
    use futures_util::StreamExt as _;

    let connection = zbus::Connection::session().await.ok()?;
    let bus = zbus::fdo::DBusProxy::new(&connection).await.ok()?;
    let mut owners = bus.receive_name_owner_changed_with_args(&[(0, PORTAL)]).await.ok()?;
    loop {
        // Listen first, so a change between the read and the subscription is not lost.
        let settings = Settings::with_connection(connection.clone()).await.ok();
        let mut changes = match &settings {
            // Every setting, not the typed accent stream: that one drops a value
            // that is not `(ddd)`, and COSMIC's is not.
            Some(settings) => settings.receive_setting_changed().await.ok(),
            None => None,
        };
        if let Some(settings) = &settings {
            let accent = settings.accent_color().await.ok().and_then(|c| Rgb::from_portal(c.red(), c.green(), c.blue()));
            cx.update(|cx| set(accent, cx));
        }
        // Follow changes until the portal starts again under a new owner.
        loop {
            let restart = owners.next();
            let owner_change = match changes.as_mut() {
                Some(stream) => match select(stream.next(), restart).await {
                    Either::Left((Some(setting), _)) => {
                        if setting.namespace() != "org.freedesktop.appearance" || setting.key() != "accent-color" {
                            continue;
                        }
                        let accent = match accent_in(setting.value()) {
                            Some(accent) => accent,
                            // A shape not known here: ask for the value instead.
                            None => match &settings {
                                Some(settings) => settings.accent_color().await.ok().and_then(|c| Rgb::from_portal(c.red(), c.green(), c.blue())),
                                None => None,
                            },
                        };
                        cx.update(|cx| set(accent, cx));
                        continue;
                    }
                    Either::Left((None, _)) => {
                        changes = None;
                        continue;
                    }
                    Either::Right((signal, _)) => signal,
                },
                None => restart.await,
            };
            let args = owner_change?.args().ok()?.new_owner().as_ref().map(|owner| owner.to_string());
            if portal_started(args.as_deref()) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What GNOME 49 on the test machine returned for its purple accent
    /// (`gdbus call ... ReadOne org.freedesktop.appearance accent-color`),
    /// digit for digit.
    #[allow(clippy::excessive_precision)]
    const GNOME_PURPLE: (f64, f64, f64) = (0.56862747669219971, 0.25490197539329529, 0.67450982332229614);

    fn hex(rgb: u32) -> Rgb {
        Rgb { r: ((rgb >> 16) & 0xff) as f32 / 255., g: ((rgb >> 8) & 0xff) as f32 / 255., b: (rgb & 0xff) as f32 / 255. }
    }

    fn light() -> ThemeColor {
        *ThemeColor::light()
    }

    fn dark() -> ThemeColor {
        *ThemeColor::dark()
    }

    #[test]
    fn the_gnome_reply_parses_to_its_colour() {
        let (r, g, b) = GNOME_PURPLE;
        let accent = Rgb::from_portal(r, g, b).expect("an accent");
        assert_eq!(accent, hex(0x9141ac), "GNOME's purple, #9141ac");
    }

    #[test]
    fn out_of_range_or_not_a_number_is_no_accent() {
        assert_eq!(Rgb::from_portal(-1., -1., -1.), None, "how the portal says none is set");
        assert_eq!(Rgb::from_portal(0.2, 1.5, 0.2), None);
        assert_eq!(Rgb::from_portal(0.2, f64::NAN, 0.2), None);
        assert_eq!(Rgb::from_portal(f64::INFINITY, 0., 0.), None);
        assert!(Rgb::from_portal(0., 0., 0.).is_some() && Rgb::from_portal(1., 1., 1.).is_some(), "the ends of the range are colours");
    }

    #[test]
    fn contrast_matches_wcag() {
        assert!((contrast(WHITE, BLACK) - 21.).abs() < 0.01);
        assert!((contrast(WHITE, WHITE) - 1.).abs() < 0.001);
        // #767676 on white is the classic 4.54:1.
        assert!((contrast(hex(0x767676), WHITE) - 4.54).abs() < 0.01);
    }

    #[test]
    fn text_on_an_accent_is_white_or_dark_and_always_readable() {
        assert_eq!(text_on(hex(0x9141ac)), WHITE, "GNOME purple takes white");
        assert_eq!(text_on(hex(0x3584e4)), NEAR_BLACK, "GNOME blue is too light for white at 4.5:1");
        assert_eq!(text_on(hex(0xf6d32d)), NEAR_BLACK, "GNOME yellow takes dark text");
        // Every grey and every primary/secondary mix: the text chosen reads.
        for v in (0..=255).step_by(5) {
            for accent in [hex(v * 0x010101), hex(v << 16), hex(v << 8), hex(v), hex(v << 16 | 0xff), hex(0xff00 | v << 16)] {
                assert!(contrast(text_on(accent), accent) >= TEXT_CONTRAST, "{accent:?}");
            }
        }
    }

    /// Every accent the desktops offer, plus the extremes.
    const ACCENTS: [u32; 15] =
        [0x3584e4, 0x2190a4, 0x3a944a, 0xc88800, 0xed5b00, 0xe62d42, 0xd56199, 0x9141ac, 0x6f8396, 0xf6d32d, 0x3daee9, 0xe8cb2d, 0x000000, 0xffffff, 0x777777];

    #[test]
    fn every_derived_colour_keeps_its_contrast_in_light_and_dark() {
        for base in [light(), dark()] {
            let (bg, fg) = (Rgb::from_hsla(base.background), Rgb::from_hsla(base.foreground));
            for accent in ACCENTS.map(hex) {
                let p = Palette::new(accent, bg, fg);
                for fill in [p.fill, p.fill_hover, p.fill_active] {
                    assert!(contrast(p.on_fill, fill) >= TEXT_CONTRAST, "text on {fill:?} for {accent:?}");
                    assert!(contrast(fill, bg) >= UI_CONTRAST, "{fill:?} against the window for {accent:?}");
                }
                assert!(contrast(p.link, bg) >= TEXT_CONTRAST, "link for {accent:?}");
                assert!(contrast(p.link_hover, bg) >= TEXT_CONTRAST, "hovered link for {accent:?}");
                assert!(contrast(p.line, bg) >= UI_CONTRAST, "ring and bars for {accent:?}");
                assert!(contrast(fg, p.selection) >= TEXT_CONTRAST, "selected text for {accent:?}");
                let mut colors = base;
                paint(&mut colors, Some(accent), &base);
                assert!(contrast(Rgb::from_hsla(colors.switch_thumb), p.fill) >= UI_CONTRAST, "a switch's thumb on {accent:?}");
            }
        }
    }

    #[test]
    fn light_and_dark_get_their_own_shades() {
        let accent = hex(0x3584e4);
        let (l, d) = (light(), dark());
        let in_light = Palette::new(accent, Rgb::from_hsla(l.background), Rgb::from_hsla(l.foreground));
        let in_dark = Palette::new(accent, Rgb::from_hsla(d.background), Rgb::from_hsla(d.foreground));
        assert_eq!(in_light.fill, accent, "buttons are the colour the user picked");
        assert_eq!(in_dark.fill, accent);
        assert!(in_light.link.luminance() < accent.luminance(), "darker links on white");
        assert!(in_light.fill.luminance() <= accent.luminance(), "never a lighter fill on white");
        assert!(in_dark.link.luminance() >= accent.luminance(), "never darker links on black");
        assert!(in_light.selection.luminance() >= accent.luminance(), "never a darker selection under dark text");
        assert!(in_dark.selection.luminance() < accent.luminance(), "a darker selection under light text");
        let purple = hex(0x9141ac);
        let selected = Palette::new(purple, Rgb::from_hsla(l.background), Rgb::from_hsla(l.foreground)).selection;
        assert!(selected.luminance() > purple.luminance(), "purple is too dark under dark text, so it is lightened");
    }

    #[test]
    fn hover_and_active_step_away_from_the_window_while_the_text_reads() {
        for accent in [0x9141ac, 0xf6d32d, 0xe8cb2d] {
            let on_white = Palette::new(hex(accent), WHITE, BLACK);
            assert!(on_white.fill_hover.luminance() < on_white.fill.luminance(), "darker on white for {accent:06x}");
            assert!(on_white.fill_active.luminance() < on_white.fill_hover.luminance());
        }
        let dark_bg = Rgb::from_hsla(dark().background);
        let yellow = Palette::new(hex(0xf6d32d), dark_bg, WHITE);
        assert!(yellow.fill_hover.luminance() > yellow.fill.luminance(), "lighter on a dark window");
        assert!(yellow.fill_active.luminance() > yellow.fill_hover.luminance());
    }

    #[test]
    fn the_accent_reaches_the_controls_and_spares_the_status_colours() {
        for base in [light(), dark()] {
            let mut colors = base;
            paint(&mut colors, Some(hex(0x9141ac)), &base);
            assert_eq!(colors.primary, hex(0x9141ac).hsla(1.));
            assert_eq!(colors.button_primary, colors.primary);
            assert_eq!(colors.primary_foreground, WHITE.hsla(1.));
            assert_ne!(colors.ring, base.ring);
            assert_ne!(colors.link, base.link);
            assert_eq!(colors.list_active.a, base.list_active.a, "the selected row stays as translucent");
            assert_eq!(colors.drop_target.a, base.drop_target.a);
            for (now, before) in [
                (colors.success, base.success),
                (colors.warning, base.warning),
                (colors.danger, base.danger),
                (colors.info, base.info),
                (colors.accent, base.accent),
                (colors.background, base.background),
                (colors.foreground, base.foreground),
                (colors.input, base.input),
            ] {
                assert_eq!(now, before);
            }
        }
    }

    #[test]
    fn no_accent_leaves_the_theme_as_it_was() {
        for base in [light(), dark()] {
            let mut colors = base;
            paint(&mut colors, None, &base);
            assert_eq!(colors, base, "no portal or no accent: the toolkit's colours, all of them");
            // And an accent that goes away puts every colour back.
            paint(&mut colors, Some(hex(0xe62d42)), &base);
            paint(&mut colors, None, &base);
            assert_eq!(colors, base);
        }
    }

    #[test]
    fn links_read_even_when_the_toolkit_dims_them() {
        for base in [light(), dark()] {
            let bg = Rgb::from_hsla(base.background);
            for accent in ACCENTS.map(hex) {
                let link = link_on(accent, bg);
                assert!(contrast(link, bg) >= LINK_CONTRAST, "{accent:?}");
                assert!(contrast(bg.mix(link, 0.8), bg) >= TEXT_CONTRAST, "hovered (80%) link for {accent:?}");
                assert!(contrast(bg.mix(link, 0.6), bg) >= TEXT_CONTRAST, "pressed (60%) link for {accent:?}");
            }
        }
    }

    #[test]
    fn pale_and_dark_accents_still_stand_out_as_fills() {
        // KDE's yellow and a near-white on white, a near-black on the dark theme.
        for (accent, base) in [(0xe8cb2d, light()), (0xf0f0f0, light()), (0x111111, dark()), (0x1a1a40, dark())] {
            let bg = Rgb::from_hsla(base.background);
            let p = Palette::new(hex(accent), bg, Rgb::from_hsla(base.foreground));
            assert!(contrast(p.fill, bg) >= UI_CONTRAST, "{accent:06x}: a checked radio or switch must be seen");
            assert!(contrast(p.on_fill, p.fill) >= TEXT_CONTRAST);
        }
        // An accent that already stands out is used as it is.
        let purple = Palette::new(hex(0x9141ac), WHITE, BLACK);
        assert_eq!(purple.fill, hex(0x9141ac));
    }

    #[test]
    fn a_live_change_paints_as_a_fresh_start_would() {
        for base in [light(), dark()] {
            for a in ACCENTS.map(hex) {
                for b in [hex(0x9141ac), hex(0x3daee9), hex(0xf6d32d), hex(0x111111)] {
                    let mut live = base;
                    paint(&mut live, Some(a), &base);
                    paint(&mut live, Some(b), &base);
                    let mut fresh = base;
                    paint(&mut fresh, Some(b), &base);
                    assert_eq!(live, fresh, "{a:?} then {b:?}");
                }
            }
        }
    }

    #[test]
    fn the_accent_is_read_from_a_tuple_or_an_array() {
        use ashpd::zbus::zvariant::Value;
        let purple = (0.569, 0.255, 0.675);
        let tuple = Value::from(purple);
        assert_eq!(accent_in(&tuple), Some(Rgb::from_portal(0.569, 0.255, 0.675)), "(ddd), as the portal documents it");
        let array = Value::from(vec![0.569, 0.255, 0.675]);
        assert_eq!(accent_in(&array), Some(Rgb::from_portal(0.569, 0.255, 0.675)), "ad, as COSMIC sends it");
        assert_eq!(accent_in(&Value::Value(Box::new(array.try_clone().unwrap()))), accent_in(&array), "wrapped in a variant");
        assert_eq!(accent_in(&Value::from((-1., -1., -1.))), Some(None), "no accent set");
        assert_eq!(accent_in(&Value::from(vec![0.5, 0.5])), None, "two channels are not a colour");
        assert_eq!(accent_in(&Value::from("purple")), None);
    }

    #[test]
    fn only_a_portal_with_an_owner_is_read_again() {
        assert!(portal_started(Some(":1.42")));
        assert!(!portal_started(Some("")), "the portal went away");
        assert!(!portal_started(None));
    }

    #[test]
    fn only_a_new_accent_repaints() {
        let green = Some(hex(0x3a944a));
        assert!(!repaints(None, None), "no accent at start: the theme is not touched");
        assert!(repaints(None, green));
        assert!(!repaints(Some(green), green), "the same accent again");
        assert!(repaints(Some(green), Some(hex(0x9141ac))));
        assert!(repaints(Some(green), None), "an accent unset again puts the theme's own colours back");
        assert!(!repaints(Some(None), None));
    }
}
