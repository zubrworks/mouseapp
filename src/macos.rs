//! AppKit helpers used outside the settings window: Finder, System Settings,
//! application lookup and picking, SF Symbols.

use std::path::Path;

use objc2::{AnyThread, ClassType, MainThreadMarker};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSColor, NSCompositingOperation, NSFont, NSImage,
    NSMutableParagraphStyle, NSRectFillUsingOperation, NSRunningApplication, NSTextAlignment, NSTextAttachment, NSModalResponseOK, NSOpenPanel, NSWorkspace,
};
use objc2_foundation::{NSArray, NSAttributedString, NSBundle, NSData, NSDictionary, NSFileManager, NSMutableAttributedString, NSRange, NSRect, NSSize, NSString, NSURL};

/// Makes this app active so that its window comes to the front. A menu bar
/// app is never active by itself, and the cooperative `activate()` is refused
/// while another app is in front (the window stayed hidden behind it, with
/// grey switches), so the activation is forced.
pub fn activate_app(mtm: MainThreadMarker) {
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// Build number (`CFBundleVersion`) of the app bundle the program runs from;
/// `None` outside a bundle (`cargo run`, tests).
pub fn bundle_build() -> Option<String> {
    let value = NSBundle::mainBundle().objectForInfoDictionaryKey(&NSString::from_str("CFBundleVersion"))?;
    value.downcast::<NSString>().ok().map(|s| s.to_string())
}

/// True while a process of the application with this bundle id runs (also
/// one that was started from another copy of the bundle).
pub fn app_is_running(bundle_id: &str) -> bool {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id))
        .iter()
        .any(|app| !app.isTerminated())
}

/// Shows the standard macOS About panel in front: name, icon and version come
/// from the bundle's `Info.plist`; without a bundle (`cargo run`) the version
/// of the crate is used. Under the version come the `license` line and a
/// GitHub icon that opens the project page.
pub fn show_about_panel(mtm: MainThreadMarker, license: &str) {
    let key = |k: &str| NSString::from_str(k);
    let bundle_version = |k: &str| NSBundle::mainBundle().objectForInfoDictionaryKey(&key(k));
    let mut keys = vec![key("ApplicationName"), key("Credits")];
    let mut values: Vec<Retained<AnyObject>> = vec![
        // SAFETY: an NSString is an object; the dictionary only retains it.
        unsafe { Retained::cast_unchecked(NSString::from_str("Mouse")) },
        // SAFETY: as above, for an NSAttributedString.
        unsafe { Retained::cast_unchecked(credits(license)) },
    ];
    if bundle_version("CFBundleShortVersionString").is_none() {
        keys.push(key("ApplicationVersion"));
        // SAFETY: as above.
        values.push(unsafe {
            Retained::cast_unchecked(NSString::from_str(env!("CARGO_PKG_VERSION")))
        });
    }
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let options = NSDictionary::from_retained_objects(&key_refs, &values);
    let app = NSApplication::sharedApplication(mtm);
    activate_app(mtm);
    // SAFETY: the options are the documented keys with NSString/NSAttributedString values.
    unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
    // Bring the About panel itself forward (not the key window, which may be
    // the settings window when the app was not active). It is found by its
    // class (`NSAboutPanel`).
    for window in app.windows() {
        if window.class().name().to_string_lossy().contains("About") {
            window.orderFrontRegardless();
            window.makeKeyAndOrderFront(None);
        }
    }
}

/// Project page opened by the GitHub icon in the About panel.
const PROJECT_URL: &str = "https://github.com/zubrworks/mouseapp";

/// GitHub mark (Primer Octicons `mark-github-16`).
///
/// MIT License. Copyright (c) 2026 GitHub Inc.
///
/// Permission is hereby granted, free of charge, to any person obtaining a
/// copy of this software and associated documentation files (the
/// "Software"), to deal in the Software without restriction, including
/// without limitation the rights to use, copy, modify, merge, publish,
/// distribute, sublicense, and/or sell copies of the Software, and to permit
/// persons to whom the Software is furnished to do so, subject to the
/// following conditions:
///
/// The above copyright notice and this permission notice shall be included
/// in all copies or substantial portions of the Software.
///
/// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
/// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
/// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
/// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
/// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
/// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
/// USE OR OTHER DEALINGS IN THE SOFTWARE.
const GITHUB_MARK_SVG: &[u8] = include_bytes!("github-mark.svg");

/// Size of the GitHub icon in the About panel, in points.
const GITHUB_ICON_SIZE: f64 = 18.0;

/// The GitHub mark in the current label color (follows light and dark mode).
fn github_icon() -> Option<Retained<NSImage>> {
    let svg = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(GITHUB_MARK_SVG))?;
    let handler = RcBlock::new(move |rect: NSRect| -> Bool {
        svg.drawInRect(rect);
        NSColor::labelColor().set();
        NSRectFillUsingOperation(rect, NSCompositingOperation::SourceIn);
        Bool::YES
    });
    let size = NSSize::new(GITHUB_ICON_SIZE, GITHUB_ICON_SIZE);
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &handler);
    image.setAccessibilityDescription(Some(&NSString::from_str("GitHub")));
    Some(image)
}

/// Text under the version in the About panel: the `license` line and, centered
/// below it, the GitHub icon as a link to the project page.
fn credits(license: &str) -> Retained<NSAttributedString> {
    let style = NSMutableParagraphStyle::new();
    style.setAlignment(NSTextAlignment::Center);
    let objs: [Retained<AnyObject>; 3] = [
        // SAFETY: font, color and style are objects stored as attribute values.
        unsafe { Retained::cast_unchecked(NSFont::systemFontOfSize(11.0)) },
        unsafe { Retained::cast_unchecked(NSColor::labelColor()) },
        unsafe { Retained::cast_unchecked(style) },
    ];
    let names = [
        NSString::from_str("NSFont"),
        NSString::from_str("NSColor"),
        NSString::from_str("NSParagraphStyle"),
    ];
    let name_refs: Vec<&NSString> = names.iter().map(|n| &**n).collect();
    let attrs: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_retained_objects(&name_refs, &objs);
    // SAFETY: `attrs` holds the documented attribute names with matching values.
    let text: Retained<NSMutableAttributedString> = unsafe {
        msg_send![NSMutableAttributedString::alloc(), initWithString: &*NSString::from_str(&format!("{license}\n")), attributes: &*attrs]
    };
    let attachment = NSTextAttachment::new();
    attachment.setImage(github_icon().as_deref());
    // SAFETY: a class method returning an attributed string for the attachment.
    let icon: Retained<NSAttributedString> =
        unsafe { msg_send![NSAttributedString::class(), attributedStringWithAttachment: &*attachment] };
    text.appendAttributedString(&icon);
    let range = NSRange::new(text.length() - icon.length(), icon.length());
    let url = NSURL::URLWithString(&NSString::from_str(PROJECT_URL));
    let tip = NSString::from_str(PROJECT_URL);
    // SAFETY: attribute values are objects; names are the documented keys.
    unsafe {
        if let Some(url) = &url {
            let _: () = msg_send![&*text, addAttribute: &*NSString::from_str("NSLink"), value: &**url, range: range];
        }
        let _: () = msg_send![&*text, addAttribute: &*NSString::from_str("NSToolTip"), value: &*tip, range: range];
        let _: () = msg_send![&*text, addAttributes: &*attrs, range: range];
    }
    Retained::into_super(text)
}

/// Shows `path` selected in Finder, or its folder when the file does not
/// exist yet. Nothing is created.
pub fn reveal_in_finder(path: &Path) {
    let workspace = NSWorkspace::sharedWorkspace();
    if path.is_file() {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        workspace.activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
    } else if let Some(dir) = path.parent() {
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&dir.to_string_lossy()), true);
        if !workspace.openURL(&url) {
            log::warn!("cannot open settings folder");
        }
    }
}

/// Opens a URL (e.g. a System Settings pane); false if it cannot be opened.
pub fn open_url(url: &str) -> bool {
    NSURL::URLWithString(&NSString::from_str(url))
        .is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url))
}

/// SF Symbol `name` with a spoken `description` (for VoiceOver).
pub fn symbol_image(name: &str, description: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )
}

/// Display name of an application ("Safari"); the bundle id when the
/// application cannot be found.
pub fn app_name(bundle: &str) -> String {
    let url = NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle));
    url.and_then(|u| u.path())
        .map(|path| {
            NSFileManager::defaultManager()
                .displayNameAtPath(&path)
                .to_string()
        })
        .unwrap_or_else(|| bundle.to_string())
}

/// Icon of an application, or the generic application icon when it cannot
/// be found.
#[allow(deprecated)] // the replacement needs UTType, which has no binding here
pub fn app_icon(bundle: &str) -> Retained<NSImage> {
    let workspace = NSWorkspace::sharedWorkspace();
    workspace
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle))
        .and_then(|u| u.path())
        .map_or_else(
            || workspace.iconForFileType(&NSString::from_str("app")),
            |path| workspace.iconForFile(&path),
        )
}

/// Running regular applications (bundle id, name), sorted by name.
pub fn running_apps() -> Vec<(String, String)> {
    let mut apps: Vec<(String, String)> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter_map(|a| {
            let id = a.bundleIdentifier()?.to_string();
            let name = a
                .localizedName()
                .map_or_else(|| id.clone(), |n| n.to_string());
            Some((id, name))
        })
        .collect();
    apps.sort_by_key(|(_, name)| name.to_lowercase());
    apps.dedup_by(|a, b| a.0 == b.0);
    apps
}

/// Bundle id of the application at `url` (from its `Info.plist`).
fn bundle_id_at(url: &NSURL) -> Option<String> {
    NSBundle::bundleWithURL(url)?
        .bundleIdentifier()
        .map(|id| id.to_string())
}

/// Lets the panel choose only application bundles (`UTType` of
/// `com.apple.application-bundle`). `UTType` is called through the runtime:
/// objc2 has no feature for it in the crates we already use.
fn restrict_to_applications(panel: &NSOpenPanel) {
    let Some(class) = AnyClass::get(c"UTType") else {
        return;
    };
    let id = NSString::from_str("com.apple.application-bundle");
    // SAFETY: `+[UTType typeWithIdentifier:]` takes an NSString and returns an
    // optional UTType; `-[NSSavePanel setAllowedContentTypes:]` takes an NSArray.
    unsafe {
        let ty: Option<Retained<AnyObject>> = msg_send![class, typeWithIdentifier: &*id];
        if let Some(ty) = ty {
            let types: Retained<NSArray<AnyObject>> = NSArray::from_retained_slice(&[ty]);
            let _: () = msg_send![panel, setAllowedContentTypes: &*types];
        }
    }
}

/// Lets the user pick an application in /Applications.
pub fn choose_app(mtm: MainThreadMarker) -> Option<String> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    let apps_dir = NSURL::fileURLWithPath(&NSString::from_str("/Applications"));
    panel.setDirectoryURL(Some(&apps_dir));
    restrict_to_applications(&panel);
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    bundle_id_at(&*panel.URL()?)
}

#[cfg(test)]
mod about_tests {
    use super::*;

    #[test]
    fn github_mark_svg_loads() {
        let icon = github_icon().expect("SVG not decoded");
        assert_eq!(icon.size().width, GITHUB_ICON_SIZE);
    }
}
