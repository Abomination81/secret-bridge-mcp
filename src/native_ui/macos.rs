//! AppKit owns the macOS dialog, typography, focus, dragging, and control rendering.
use std::path::Path;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect,
    NSSize, NSString, ns_string,
};
use zeroize::Zeroizing;

use super::{
    Mode, PROMPT_CANCELLED_EXIT_CODE, SERVICE_NAME, SecureInputGuard, credential_text,
    ensure_entry_is_empty,
};

/// Width of the credential dialog's accessory content.
const CONTENT_WIDTH: f64 = 300.0;
const FIELD_HEIGHT: f64 = 28.0;
const WELL_HEIGHT: f64 = 60.0;
const LOCK_SIDE: f64 = 11.0;

struct InputState {
    field: Retained<NSSecureTextField>,
    submit: Retained<NSButton>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. All UI state stays on the main thread.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = InputState]
    struct InputDelegate;

    unsafe impl NSObjectProtocol for InputDelegate {}
    unsafe impl NSControlTextEditingDelegate for InputDelegate {
        #[unsafe(method(controlTextDidChange:))]
        fn changed(&self, _notification: &NSNotification) {
            let state = self.ivars();
            let value = Zeroizing::new(state.field.stringValue().to_string());
            let credential = credential_text(&value);
            state
                .submit
                .setEnabled(!credential.is_empty() && credential.len() <= 65_536);
            // Clear a pasted value without retaining or exposing clipboard contents. AppKit's
            // field editor owns the paste action, so inspect only after the field has changed.
            let pasteboard = NSPasteboard::generalPasteboard();
            if let Some(text) = pasteboard.stringForType(unsafe { NSPasteboardTypeString }) {
                let pasted = Zeroizing::new(text.to_string());
                if !pasted.is_empty() && value.contains(pasted.as_str()) {
                    pasteboard.clearContents();
                }
            }
        }
    }
    unsafe impl NSTextFieldDelegate for InputDelegate {}
);

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn srgb(red: f64, green: f64, blue: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(red / 255.0, green / 255.0, blue / 255.0, 1.0)
}

/// The SecretBridge app icon: a white padlock under a bridge arch on a green tile. It is drawn
/// on demand, so it stays sharp at every backing scale without a bundled image file.
fn app_icon(side: f64) -> Retained<NSImage> {
    let draw = RcBlock::new(|bounds: NSRect| -> Bool {
        // A 64-point design grid in a flipped context, so y grows downward.
        let unit = bounds.size.width / 64.0;
        let at = |x: f64, y: f64| NSPoint::new(x * unit, y * unit);
        let frame = |x: f64, y: f64, width: f64, height: f64| {
            rect(x * unit, y * unit, width * unit, height * unit)
        };
        let deep_green = srgb(27.0, 154.0, 71.0);
        let tile = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            frame(2.0, 2.0, 60.0, 60.0),
            14.0 * unit,
            14.0 * unit,
        );
        if let Some(gradient) = NSGradient::initWithStartingColor_endingColor(
            NSGradient::alloc(),
            &srgb(95.0, 219.0, 118.0),
            &deep_green,
        ) {
            gradient.drawInBezierPath_angle(&tile, 90.0);
        }
        NSColor::whiteColor().set();
        let arch = NSBezierPath::bezierPath();
        arch.moveToPoint(at(11.0, 35.0));
        arch.curveToPoint_controlPoint1_controlPoint2(
            at(53.0, 35.0),
            at(17.0, 15.0),
            at(47.0, 15.0),
        );
        arch.setLineWidth(4.0 * unit);
        arch.setLineCapStyle(NSLineCapStyle::Round);
        arch.stroke();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            frame(20.0, 30.0, 24.0, 20.0),
            5.0 * unit,
            5.0 * unit,
        )
        .fill();
        deep_green.set();
        NSBezierPath::bezierPathWithOvalInRect(frame(29.4, 35.9, 5.2, 5.2)).fill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            frame(30.8, 39.5, 2.4, 6.0),
            1.2 * unit,
            1.2 * unit,
        )
        .fill();
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(side, side), true, &draw)
}

const ALIGNMENT_PROBE: &str = "SecretBridge layout probe";

/// How this system aligns alert text: flush with the leading edge on macOS 26 with its SDK,
/// centered on earlier releases and for older SDKs. Read from a throwaway alert's own title so
/// the accessory always matches the alert around it, with the OS version as a fallback.
fn alert_alignment(mtm: MainThreadMarker) -> NSTextAlignment {
    let probe = NSAlert::new(mtm);
    probe.setMessageText(&NSString::from_str(ALIGNMENT_PROBE));
    probe.layout();
    let centered = match probe
        .window()
        .contentView()
        .and_then(|view| probe_title_alignment(&view))
    {
        Some(alignment) => alignment == NSTextAlignment::Center,
        None => {
            NSProcessInfo::processInfo()
                .operatingSystemVersion()
                .majorVersion
                < 26
        }
    };
    if centered {
        NSTextAlignment::Center
    } else {
        NSTextAlignment::Natural
    }
}

fn probe_title_alignment(view: &NSView) -> Option<NSTextAlignment> {
    for subview in view.subviews().iter() {
        if let Some(field) = subview.downcast_ref::<NSTextField>()
            && field.stringValue().to_string() == ALIGNMENT_PROBE
        {
            return Some(field.alignment());
        }
        if let Some(alignment) = probe_title_alignment(&subview) {
            return Some(alignment);
        }
    }
    None
}

fn single_line(
    mtm: MainThreadMarker,
    text: &str,
    font: &NSFont,
    secondary: bool,
) -> Retained<NSTextField> {
    let line = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    line.setFont(Some(font));
    if secondary {
        line.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    if let Some(cell) = line.cell() {
        cell.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    }
    line
}

/// The width a label needs to draw its text on one line.
fn natural_width(label: &NSTextField) -> f64 {
    label.cell().map_or(f64::MAX, |cell| {
        cell.cellSizeForBounds(rect(0.0, 0.0, 1_000_000.0, 1_000_000.0))
            .width
            .ceil()
    })
}

fn tilde_path(path: &Path) -> String {
    match std::env::var_os("HOME").map(|home| path.strip_prefix(home)) {
        Some(Ok(rest)) if rest.as_os_str().is_empty() => "~".into(),
        Some(Ok(rest)) => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// A recessed well naming the workspace that Save & Use authorizes: the folder name, then its
/// full location on a line of its own so a long name can never hide where the folder is.
fn workspace_well(mtm: MainThreadMarker, workspace: &str) -> Retained<NSBox> {
    let well = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        rect(0.0, 0.0, CONTENT_WIDTH, WELL_HEIGHT),
    );
    well.setBoxType(NSBoxType::Custom);
    well.setBorderWidth(0.0);
    well.setTitlePosition(NSTitlePosition::NoTitle);
    well.setCornerRadius(8.0);
    // The quaternary label tint reads as a recessed well in both light and dark mode.
    well.setFillColor(&NSColor::quaternaryLabelColor());
    well.setContentViewMargins(NSSize::new(0.0, 0.0));
    well.setToolTip(Some(&NSString::from_str(workspace)));
    let Some(content) = well.contentView() else {
        return well;
    };

    if let Some(folder) = NSImage::imageNamed(unsafe { NSImageNameFolder }) {
        let icon = NSImageView::imageViewWithImage(&folder, mtm);
        icon.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        icon.setFrame(rect(10.0, 16.0, 28.0, 28.0));
        content.addSubview(&icon);
    }
    let text_x = 46.0;
    let text_width = CONTENT_WIDTH - text_x - 10.0;
    let caption_font = NSFont::systemFontOfSize_weight(10.0, unsafe { NSFontWeightSemibold });
    let caption = single_line(mtm, "USE IN", &caption_font, true);
    caption.setFrame(rect(text_x, 40.0, text_width, 13.0));
    content.addSubview(&caption);

    let path = Path::new(workspace);
    let name = path.file_name().map_or_else(
        || workspace.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let name_font = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightMedium });
    let folder_name = single_line(mtm, &name, &name_font, false);
    folder_name.setFrame(rect(text_x, 23.0, text_width, 16.0));
    content.addSubview(&folder_name);
    let location = single_line(
        mtm,
        &tilde_path(path),
        &NSFont::systemFontOfSize(11.0),
        true,
    );
    location.setFrame(rect(text_x, 7.0, text_width, 15.0));
    content.addSubview(&location);
    well
}

/// The storage note under the controls, led by a small lock when the note fits on one line.
fn footer_note(
    mtm: MainThreadMarker,
    text: &str,
    tooltip: &NSString,
    alignment: NSTextAlignment,
) -> Retained<NSView> {
    let note = label(mtm, text, rect(0.0, 0.0, CONTENT_WIDTH, 1.0), 11.0, true);
    note.setAlignment(alignment);
    note.setToolTip(Some(tooltip));
    let height = fit_label(&note, CONTENT_WIDTH);
    let container =
        NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, CONTENT_WIDTH, height));
    let text_width = natural_width(&note);
    let row_width = LOCK_SIDE + 4.0 + text_width;
    let lock =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(ns_string!("lock.fill"), None);
    if let Some(lock) = lock.filter(|_| row_width <= CONTENT_WIDTH) {
        let left = if alignment == NSTextAlignment::Center {
            ((CONTENT_WIDTH - row_width) / 2.0).floor()
        } else {
            0.0
        };
        let icon = NSImageView::imageViewWithImage(&lock, mtm);
        icon.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        icon.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        icon.setFrame(rect(
            left,
            ((height - LOCK_SIDE) / 2.0).floor(),
            LOCK_SIDE,
            LOCK_SIDE,
        ));
        icon.setToolTip(Some(tooltip));
        container.addSubview(&icon);
        note.setAlignment(NSTextAlignment::Natural);
        note.setFrame(rect(left + LOCK_SIDE + 4.0, 0.0, text_width, height));
    }
    container.addSubview(&note);
    container
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    frame: NSRect,
    size: f64,
    secondary: bool,
) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setFrame(frame);
    label.setPreferredMaxLayoutWidth(frame.size.width);
    label.setFont(Some(&NSFont::systemFontOfSize(size)));
    label.setMaximumNumberOfLines(0);
    if let Some(cell) = label.cell() {
        cell.setUsesSingleLineMode(false);
        cell.setScrollable(false);
        cell.setWraps(true);
        cell.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        cell.setTruncatesLastVisibleLine(false);
    }
    if secondary {
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    label
}

fn fit_label(label: &NSTextField, width: f64) -> f64 {
    label.setPreferredMaxLayoutWidth(width);
    // Measure the wrapped cell at its actual drawing width, including explicit line breaks.
    // fittingSize alone describes Auto Layout's preferred size, not this manual text layout.
    let height = label
        .cell()
        .map(|cell| {
            cell.cellSizeForBounds(rect(0.0, 0.0, width, 1_000_000.0))
                .height
        })
        .unwrap_or_else(|| label.fittingSize().height)
        .ceil()
        + 2.0;
    label.setFrameSize(NSSize::new(width, height));
    height
}

fn message_view(
    mtm: MainThreadMarker,
    text: &str,
    width: f64,
    max_height: f64,
    size: f64,
    secondary: bool,
    alignment: NSTextAlignment,
) -> Retained<NSScrollView> {
    let details = label(mtm, text, rect(0.0, 0.0, width, 1.0), size, secondary);
    details.setAlignment(alignment);
    let natural_height = fit_label(&details, width);
    let overflow = natural_height > max_height;
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(0.0, 0.0, width, natural_height.min(max_height)),
    );
    scroll.setBorderType(NSBorderType::NoBorder);
    scroll.setDrawsBackground(false);
    scroll.setHasHorizontalScroller(false);
    // A persistent scrollbar makes overflow discoverable even with macOS auto-hide enabled.
    scroll.setScrollerStyle(NSScrollerStyle::Legacy);
    scroll.setAutohidesScrollers(false);
    scroll.setHasVerticalScroller(overflow);
    fit_label(&details, scroll.contentSize().width);
    scroll.setDocumentView(Some(&details));
    let clip = scroll.contentView();
    let top = if details.isFlipped() {
        0.0
    } else {
        (details.frame().size.height - scroll.contentSize().height).max(0.0)
    };
    clip.scrollToPoint(NSPoint::new(0.0, top));
    scroll.reflectScrolledClipView(&clip);
    scroll
}

fn install_edit_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let main = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    // SAFETY: These standard AppKit actions use the responder chain with the normal sender argument.
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Quit SecretBridge"),
            Some(sel!(terminate:)),
            ns_string!("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    main.addItem(&app_item);
    let edit_item = NSMenuItem::new(mtm);
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    for (title, action, key) in [
        ("Cut", sel!(cut:), "x"),
        ("Copy", sel!(copy:), "c"),
        ("Paste", sel!(paste:), "v"),
        ("Select All", sel!(selectAll:), "a"),
    ] {
        // SAFETY: All selectors are standard text-editing actions with a single sender argument.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        edit.addItem(&item);
    }
    edit_item.setSubmenu(Some(&edit));
    main.addItem(&edit_item);
    app.setMainMenu(Some(&main));
}

pub(super) fn run(mode: Mode, secure_input: Option<SecureInputGuard>) -> Result<u8, String> {
    let mtm = MainThreadMarker::new().ok_or("native dialog must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_edit_menu(mtm, &app);
    app.finishLaunching();
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    let icon = app_icon(64.0);
    icon.setAccessibilityDescription(Some(ns_string!("SecretBridge")));
    // SAFETY: The image is created and configured on this same main thread.
    unsafe {
        alert.setIcon(Some(&icon));
    }
    let alignment = alert_alignment(mtm);
    let is_secret = matches!(mode, Mode::Secret { .. });
    let legacy = matches!(&mode, Mode::Secret { workspace, .. } if workspace.is_empty());
    let submit = alert.addButtonWithTitle(if legacy {
        ns_string!("Save")
    } else if is_secret {
        ns_string!("Save & Use")
    } else {
        ns_string!("Allow")
    });
    let cancel = alert.addButtonWithTitle(ns_string!("Cancel"));
    cancel.setKeyEquivalent(ns_string!("\u{1b}"));
    let mut field = None;
    let mut input_delegate = None;

    match &mode {
        Mode::Secret {
            label: title,
            description,
            workspace,
            client,
            env_var,
            replacing,
            ..
        } => {
            alert.setMessageText(&NSString::from_str(if *replacing {
                "Replace credential"
            } else {
                "Save a credential"
            }));
            alert.setInformativeText(&NSString::from_str(title));
            // Stack bottom-up in AppKit coordinates: note, workspace well, field, details.
            let note = footer_note(
                mtm,
                &if legacy {
                    "Saved in Keychain. Never sent to chat.".into()
                } else {
                    format!("Saved in Keychain. Available to {client} here.")
                },
                ns_string!(
                    "Save & Use authorizes .env writes in this workspace without another prompt. Client names are supplied by the requesting process and are not verified. The credential is never sent to chat; local processes can read exported files."
                ),
                alignment,
            );
            let mut next_y = note.frame().size.height + 12.0;
            // Older brokers send no workspace, so there is no destination to show.
            let well = (!legacy).then(|| workspace_well(mtm, workspace));
            if let Some(well) = &well {
                well.setFrameOrigin(NSPoint::new(0.0, next_y));
                next_y += WELL_HEIGHT + 8.0;
            }
            let input_y = next_y;
            let details_y = input_y + FIELD_HEIGHT + 12.0;
            let max_height = NSScreen::mainScreen(mtm).map_or(220.0, |screen| {
                (screen.visibleFrame().size.height - 420.0).clamp(80.0, 220.0)
            });
            let scroll = message_view(
                mtm,
                description,
                CONTENT_WIDTH,
                max_height,
                11.0,
                true,
                alignment,
            );
            scroll.setFrameOrigin(NSPoint::new(0.0, details_y));
            let accessory = NSView::initWithFrame(
                NSView::alloc(mtm),
                rect(
                    0.0,
                    0.0,
                    CONTENT_WIDTH,
                    details_y + scroll.frame().size.height,
                ),
            );
            accessory.addSubview(&scroll);

            let input = NSSecureTextField::initWithFrame(
                NSSecureTextField::alloc(mtm),
                rect(0.0, input_y, CONTENT_WIDTH, FIELD_HEIGHT),
            );
            input.setFont(Some(&NSFont::systemFontOfSize(13.0)));
            input.setControlSize(NSControlSize::Large);
            input.setPlaceholderString(Some(ns_string!("API key or password")));
            input.setBezelStyle(NSTextFieldBezelStyle::RoundedBezel);
            if let Some(name) = env_var {
                input.setToolTip(Some(&NSString::from_str(name)));
            }
            accessory.addSubview(&input);
            if let Some(well) = &well {
                accessory.addSubview(well);
            }
            accessory.addSubview(&note);
            alert.setAccessoryView(Some(&accessory));
            submit.setEnabled(false);
            let delegate = InputDelegate::alloc(mtm).set_ivars(InputState {
                field: input.clone(),
                submit: submit.clone(),
            });
            // SAFETY: The allocated NSObject subclass has initialized ivars and uses NSObject init.
            let delegate: Retained<InputDelegate> = unsafe { msg_send![super(delegate), init] };
            unsafe {
                input.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            }
            field = Some(input);
            input_delegate = Some(delegate);
        }
        Mode::Confirm {
            client,
            title,
            message,
            ..
        } => {
            alert.setMessageText(&NSString::from_str(title));
            alert.setInformativeText(&NSString::from_str(&format!("Requested by {client}")));
            let max_height = NSScreen::mainScreen(mtm).map_or(260.0, |screen| {
                (screen.visibleFrame().size.height - 340.0).clamp(80.0, 260.0)
            });
            let scroll = message_view(
                mtm,
                message,
                340.0,
                max_height,
                13.0,
                false,
                NSTextAlignment::Natural,
            );
            alert.setAccessoryView(Some(&scroll));
            // Do not let Return approve deletion or a new workspace by accident.
            submit.setKeyEquivalent(ns_string!(""));
        }
    }
    alert.layout();
    let window = alert.window();
    // The dialog keeps its system shadow and follows the user's light or dark appearance.
    window.setTitle(ns_string!("SecretBridge"));
    window.setMovable(true);
    window.setMovableByWindowBackground(true);
    window.setLevel(3); // NSFloatingWindowLevel: stays findable while copying from another app.
    window.center();
    if let Some(input) = &field {
        window.setInitialFirstResponder(Some(input));
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let response = alert.runModal();
    window.setIgnoresMouseEvents(true);
    window.orderOut(None);
    drop(input_delegate);
    if response != NSAlertFirstButtonReturn {
        if let Some(input) = &field {
            input.setStringValue(ns_string!(""));
        }
        return Ok(PROMPT_CANCELLED_EXIT_CODE);
    }
    if let Mode::Secret { secret_id, .. } = mode {
        let input = field.ok_or("credential field was unavailable")?;
        let value = Zeroizing::new(input.stringValue().to_string());
        input.setStringValue(ns_string!(""));
        let credential = credential_text(&value);
        if credential.is_empty() || credential.len() > 65_536 {
            return Err("invalid credential length".into());
        }
        let entry = keyring::Entry::new(SERVICE_NAME, &secret_id)
            .map_err(|_| "could not access the operating-system credential store")?;
        ensure_entry_is_empty(&entry)?;
        entry
            .set_password(credential)
            .map_err(|_| "could not store the credential")?;
    }
    drop(secure_input);
    Ok(0)
}
