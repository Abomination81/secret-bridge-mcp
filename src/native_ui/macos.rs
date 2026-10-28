//! AppKit owns the macOS dialog, typography, focus, dragging, and control rendering.
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSURL, ns_string,
};
use zeroize::Zeroizing;

use super::{
    Mode, PROMPT_CANCELLED_EXIT_CODE, SERVICE_NAME, SecureInputGuard, ensure_entry_is_empty,
};

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
            state
                .submit
                .setEnabled(!value.is_empty() && value.len() <= 65_536);
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
    secondary: bool,
) -> Retained<NSScrollView> {
    let details = label(mtm, text, rect(0.0, 0.0, width, 1.0), 13.0, secondary);
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
    let symbol = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        ns_string!("lock.fill"),
        Some(ns_string!("Secure credential")),
    );
    if let Some(symbol) = symbol {
        let config = NSImageSymbolConfiguration::configurationWithHierarchicalColor(
            &NSColor::secondaryLabelColor(),
        );
        let icon = symbol.imageWithSymbolConfiguration(&config);
        if let Some(icon) = &icon {
            icon.setSize(NSSize::new(28.0, 28.0));
        }
        // SAFETY: The image is created and configured on this same main thread.
        unsafe {
            alert.setIcon(icon.as_deref());
        }
    }
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
            let note = label(
                mtm,
                &if legacy {
                    "Saved in Keychain. Never sent to chat.".into()
                } else {
                    format!("Saved in Keychain. Available to {client} here.")
                },
                rect(0.0, 0.0, 320.0, 1.0),
                11.0,
                true,
            );
            note.setAlignment(NSTextAlignment::Center);
            let note_height = fit_label(&note, 320.0);
            let scope_y = note_height + 12.0;
            let input_y = scope_y + 64.0 + 16.0;
            let details_y = input_y + 32.0 + 16.0;
            let max_height = NSScreen::mainScreen(mtm).map_or(220.0, |screen| {
                (screen.visibleFrame().size.height - 420.0).clamp(80.0, 220.0)
            });
            let scroll = message_view(mtm, description, 320.0, max_height, true);
            scroll.setFrameOrigin(NSPoint::new(0.0, details_y));
            let accessory = NSView::initWithFrame(
                NSView::alloc(mtm),
                rect(0.0, 0.0, 320.0, details_y + scroll.frame().size.height),
            );
            accessory.addSubview(&scroll);

            let input = NSSecureTextField::initWithFrame(
                NSSecureTextField::alloc(mtm),
                rect(0.0, input_y, 320.0, 32.0),
            );
            input.setFont(Some(&NSFont::systemFontOfSize(15.0)));
            input.setPlaceholderString(Some(ns_string!("API key or password")));
            input.setBezelStyle(NSTextFieldBezelStyle::RoundedBezel);
            if let Some(name) = env_var {
                input.setToolTip(Some(&NSString::from_str(name)));
            }
            accessory.addSubview(&input);

            let scope = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, scope_y, 320.0, 64.0));
            scope.setBoxType(NSBoxType::Custom);
            scope.setBorderWidth(0.0);
            scope.setTitlePosition(NSTitlePosition::NoTitle);
            scope.setCornerRadius(10.0);
            scope.setFillColor(&NSColor::controlColor());
            scope.setContentViewMargins(NSSize::new(0.0, 0.0));
            let caption = label(mtm, "USE IN", rect(12.0, 39.0, 296.0, 14.0), 10.0, true);
            caption.setFont(Some(&NSFont::systemFontOfSize_weight(10.0, unsafe {
                NSFontWeightSemibold
            })));
            let path = NSPathControl::initWithFrame(
                NSPathControl::alloc(mtm),
                rect(10.0, 9.0, 300.0, 26.0),
            );
            path.setURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(
                workspace,
            ))));
            path.setEditable(false);
            path.setToolTip(Some(&NSString::from_str(workspace)));
            if let Some(content) = scope.contentView() {
                content.addSubview(&caption);
                content.addSubview(&path);
            }
            accessory.addSubview(&scope);
            note.setToolTip(Some(ns_string!("Save & Use authorizes .env writes in this workspace without another prompt. Client names are supplied by the requesting process and are not verified. The credential is never sent to chat; local processes can read exported files.")));
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
        } => {
            alert.setMessageText(&NSString::from_str(title));
            alert.setInformativeText(&NSString::from_str(&format!("Requested by {client}")));
            let max_height = NSScreen::mainScreen(mtm).map_or(260.0, |screen| {
                (screen.visibleFrame().size.height - 340.0).clamp(80.0, 260.0)
            });
            let scroll = message_view(mtm, message, 340.0, max_height, false);
            alert.setAccessoryView(Some(&scroll));
            // Do not let Return approve deletion or a new workspace by accident.
            submit.setKeyEquivalent(ns_string!(""));
        }
    }
    alert.layout();
    let window = alert.window();
    window.setTitle(ns_string!("SecretBridge"));
    window.setHasShadow(false);
    window.setMovable(true);
    window.setMovableByWindowBackground(true);
    window.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua }).as_deref());
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
        if value.is_empty() || value.len() > 65_536 {
            return Err("invalid credential length".into());
        }
        let entry = keyring::Entry::new(SERVICE_NAME, &secret_id)
            .map_err(|_| "could not access the operating-system credential store")?;
        ensure_entry_is_empty(&entry)?;
        entry
            .set_password(&value)
            .map_err(|_| "could not store the credential")?;
    }
    drop(secure_input);
    Ok(0)
}
