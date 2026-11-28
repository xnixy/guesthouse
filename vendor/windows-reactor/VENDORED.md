# windows-reactor, vendored and patched

Microsoft's `windows-reactor` (declarative WinUI 3 for Rust, MIT OR Apache-2.0: `license-mit`,
`license-apache-2.0`), from the windows-rs repository at commit
`63a5e3d9b9afbd57c382d0a4f5a367f134e9dc25` (`crates/libs/reactor`). That commit, not the
crates.io release, because the release lacks the notification icon, reopenable windows and the
window handle. The Windows apps (`windows/app`, `windows/setup`) use this copy by path, and its own
`windows-*` dependencies are the sibling copies in `vendor/windows-rs`; the workspace's `Cargo.toml`
leaves both folders out of its members.

It was copied from the owner's blurman project (its `reactor-spike` worktree,
`vendor/windows-reactor`), which made the first two patches. Each patched place says so in a
comment ("Spike patch" for blurman's, "remoteman patch" for ours).

1. **Backdrops that stay active** (blurman): `src/native/winui/window/active_backdrop.rs` (new)
   and `src/native/winui/window/mod.rs`. WinUI's `MicaBackdrop` and `DesktopAcrylicBackdrop`
   turn to a flat colour when another window is active; this drives `MicaController` /
   `DesktopAcrylicController` directly with a `SystemBackdropConfiguration` whose
   `IsInputActive` is always true, following the window's theme.
2. **More theme brushes** (blurman): `src/declaration.rs`, `ThemeBrush::SecondaryText`,
   `TertiaryText`, `SubtleSecondary`, `Divider`, `SystemCaution`, `SystemCautionBackground`,
   `SystemSuccess` (WinUI's `TextFillColorSecondaryBrush` and so on).
3. **Notifications from a notification icon** (remoteman): `ApplicationContext::notify(icon,
   title, text)` in `src/application.rs`, carried out in `src/native/app/application.rs`, with
   `NotifyIcon::notify` and `NIF_INFO` in `src/native/notifyicon/`. A balloon
   (`Shell_NotifyIconW(NIM_MODIFY)` with `NIF_INFO`), which Windows 10 and 11 show as a toast.
4. **The backdrop let go of when the window closes** (remoteman, fixing patch 1):
   `src/native/winui/window/active_backdrop.rs` removes its target and closes the controller in
   the window's `Closed` event, as the WinUI Gallery's backdrop sample does. Reactor tears a
   closed window's content down later, after WinUI's window has gone; the controller let go of
   then crashed the process (access violation in `Microsoft.ui.xaml.dll`). It only shows in an
   app that lives on after its window closes (a notification-area app).
5. **No `ContentDialog::Hide` on a closed window** (remoteman): `WinUiAdapter`'s `Drop`
   (`src/native/winui/mod.rs`) hides the dialogs still open only if one of its windows is still
   open (`NativeWindowState::is_closed`, `src/native/winui/window/mod.rs`). Hiding a dialog of a
   window that had closed with the dialog open crashed the same way.

6. **Check and radio menu items, and dressed items** (remoteman, for the viewer's session
   menus): `MenuItem::Check` (WinUI's `ToggleMenuFlyoutItem`), `MenuItem::Radio`
   (`RadioMenuFlyoutItem`, with a group name) and `MenuItem::Dressed` (an item, check item or
   radio item with a Segoe Fluent Icons glyph, a tooltip, or a keyboard shortcut's text after
   the label), with `MenuItem::check`, `radio`, `enabled`, `glyph`, `tooltip`,
   `accelerator_text` and `key` in `src/declaration.rs`. A click reports the item's key as
   before; the app says from then on whether it is checked. The native items are made in the
   new `src/native/menu_items.rs`, which both `build_menu_items` (`src/native/winui/
   attachments.rs` for menus on controls, `src/native/transient_menu.rs` for the notification
   icon's) call for every entry that is clicked; `src/ir.rs` validates their keys. The new
   bindings (the two classes, their interfaces and factories) and `IMenuFlyoutItem`'s
   `put_Icon` and `put_KeyboardAcceleratorTextOverride` (typed in `src/native/bindings.rs`) are
   windows-bindgen's output over this commit's winmd, not written by hand.

7. **The wheel and the pointer's device** (remoteman, for the viewer's picture): `Border::
   on_pointer_wheel_changed` (WinUI's `PointerWheelChanged`; `EventId::PointerWheelChanged` in
   `src/generated.rs`, its subscription in `src/native/generated.rs` copied from
   `PointerExited`'s, the setter in `src/generated_declarations.rs`), and three fields of
   `PointerEventInfo` (`src/declaration.rs`, filled in `src/native/winui/events.rs`): `device`
   (`PointerDevice::Mouse`, `Touch` or `Pen`, from the pointer's `PointerDeviceType`),
   `wheel_delta` and `is_horizontal_wheel`. The bindings typed in `src/native/bindings.rs`
   (`IUIElement`'s wheel event, `IPointerPointProperties`' `MouseWheelDelta` and
   `IsHorizontalMouseWheel`, `IPointer`'s `PointerDeviceType` and that enum) are
   windows-bindgen's output. Upstream's pointer events had neither, and the viewer sends the
   wheel to the other computer and tells fingers from the mouse.

8. **In-app acrylic** (remoteman, for the viewer's bar over the picture): `ThemeBrush::
   AcrylicInApp` (`AcrylicInAppFillColorDefaultBrush`) in `src/declaration.rs`, beside patch 2's
   brushes.
9. **A hidden pointer** (remoteman, for the viewer's captured pointer): `InputSystemCursorShape::
   Hidden` (in `src/generated_declarations.rs`, its variant in `src/generated.rs`'s contract, and
   `Border`'s setter in `src/native/generated.rs`) wears the app's own cursor resource 1 through
   WinUI's `InputDesktopResourceCursor.CreateFromModule` (`src/native/resource_cursor.rs`, bound
   by hand: the generated bindings lack it; its IID from the runtime's `Microsoft.UI.winmd`). The
   app makes that resource blank. WinUI has no system shape for "none", and over its content it
   sets the cursor itself whatever the window answers to `WM_SETCURSOR` or `ShowCursor` says.
   If the cursor can't be made, the default one is worn (Reactor would otherwise unwrap the error).

Cargo.toml: the `[lints]` and docs.rs sections are gone (with `[lints.rust]` set here instead)
and the dependencies are paths to the sibling copies in `vendor/windows-rs`. `public-api.txt` is
upstream's and does not list the patches.

To move to a newer Reactor: copy `crates/libs/reactor` from that commit over this folder, apply
the patches again (and check whether 4 and 5 are still needed: close a window with Mica, and one
with a dialog open, in an app that keeps running), and change the commit in the workspace's
`Cargo.toml` and here.
