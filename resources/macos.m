// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

#import <AppKit/AppKit.h>
#import <stdint.h>

// AppKit and the GUI run on the main thread. Rust consumes each request once
// during its next UI update; no C pointer is retained after initialization.
static uint32_t pending_actions = 0;
static NSString *app_version = @"0.1.0";
static NSImage *app_icon = nil;

enum {
    ACTION_OPEN_DATABASE = 1 << 0,
    ACTION_EXPORT_CSV = 1 << 1,
    ACTION_DISCOVER = 1 << 2,
    ACTION_FILTER = 1 << 3,
    ACTION_READ = 1 << 4,
    ACTION_WRITE = 1 << 5,
    ACTION_TOGGLE_CONNECTION = 1 << 6,
    ACTION_CONNECTION_SETTINGS = 1 << 7,
};

// The winit view is not an AppKit text view. Deliver standard edit-menu
// shortcuts back to it as key events so egui's focused text field handles
// selection, undo, clipboard and paste exactly as keyboard shortcuts do.
static void forward_edit_action(id target, unsigned short key_code,
                                NSString *characters, NSEventModifierFlags extra_flags) {
    NSEvent *current = [NSApp currentEvent];
    if (current && [current type] == NSEventTypeKeyDown) {
        if ([target respondsToSelector:@selector(keyDown:)]) [target keyDown:current];
        return;
    }
    NSWindow *window = [NSApp keyWindow];
    if (!window || ![target respondsToSelector:@selector(keyDown:)]) return;
    NSEventModifierFlags flags = NSEventModifierFlagCommand | extra_flags;
    NSTimeInterval now = [[NSProcessInfo processInfo] systemUptime];
    NSEvent *down = [NSEvent keyEventWithType:NSEventTypeKeyDown location:NSZeroPoint
                              modifierFlags:flags timestamp:now windowNumber:[window windowNumber]
                                    context:nil characters:characters
                   charactersIgnoringModifiers:characters isARepeat:NO keyCode:key_code];
    NSEvent *up = [NSEvent keyEventWithType:NSEventTypeKeyUp location:NSZeroPoint
                            modifierFlags:flags timestamp:now windowNumber:[window windowNumber]
                                  context:nil characters:characters
                 charactersIgnoringModifiers:characters isARepeat:NO keyCode:key_code];
    [target keyDown:down];
    if ([target respondsToSelector:@selector(keyUp:)]) [target keyUp:up];
}

@implementation NSView (DevknxEditSupport)
- (void)undo:(id)sender { (void)sender; forward_edit_action(self, 6, @"z", 0); }
- (void)redo:(id)sender { (void)sender; forward_edit_action(self, 6, @"Z", NSEventModifierFlagShift); }
- (void)cut:(id)sender { (void)sender; forward_edit_action(self, 7, @"x", 0); }
- (void)copy:(id)sender { (void)sender; forward_edit_action(self, 8, @"c", 0); }
- (void)paste:(id)sender { (void)sender; forward_edit_action(self, 9, @"v", 0); }
- (void)selectAll:(id)sender { (void)sender; forward_edit_action(self, 0, @"a", 0); }
@end

@interface DevknxMenuHandler : NSObject
- (void)showAbout:(id)sender;
- (void)openHelp:(id)sender;
- (void)openDatabase:(id)sender;
- (void)exportCsv:(id)sender;
- (void)discover:(id)sender;
- (void)focusFilter:(id)sender;
- (void)readGroup:(id)sender;
- (void)writeGroup:(id)sender;
- (void)toggleConnection:(id)sender;
- (void)connectionSettings:(id)sender;
@end

@implementation DevknxMenuHandler
- (void)showAbout:(id)sender {
    (void)sender;
    NSMutableDictionary *options = [NSMutableDictionary dictionary];
    options[NSAboutPanelOptionApplicationName] = @"devknx";
    options[NSAboutPanelOptionApplicationVersion] = app_version;
    options[NSAboutPanelOptionVersion] = [NSString stringWithFormat:@"v%@", app_version];
    options[@"Copyright"] = @"Copyright © 2026 Fabian Schmieder\nGPL-3.0-only\nhttps://github.com/metaneutrons/devknx";
    if (app_icon) options[NSAboutPanelOptionApplicationIcon] = app_icon;
    [NSApp orderFrontStandardAboutPanelWithOptions:options];
    [NSApp activateIgnoringOtherApps:YES];
}
- (void)openHelp:(id)sender {
    (void)sender;
    [[NSWorkspace sharedWorkspace] openURL:[NSURL URLWithString:@"https://github.com/metaneutrons/devknx"]];
}
- (void)openDatabase:(id)sender { (void)sender; pending_actions |= ACTION_OPEN_DATABASE; }
- (void)exportCsv:(id)sender { (void)sender; pending_actions |= ACTION_EXPORT_CSV; }
- (void)discover:(id)sender { (void)sender; pending_actions |= ACTION_DISCOVER; }
- (void)focusFilter:(id)sender { (void)sender; pending_actions |= ACTION_FILTER; }
- (void)readGroup:(id)sender { (void)sender; pending_actions |= ACTION_READ; }
- (void)writeGroup:(id)sender { (void)sender; pending_actions |= ACTION_WRITE; }
- (void)toggleConnection:(id)sender { (void)sender; pending_actions |= ACTION_TOGGLE_CONNECTION; }
- (void)connectionSettings:(id)sender { (void)sender; pending_actions |= ACTION_CONNECTION_SETTINGS; }
@end

static DevknxMenuHandler *menu_handler = nil;

bool devknx_take_menu_action(uint32_t action) {
    bool requested = (pending_actions & action) != 0;
    pending_actions &= ~action;
    return requested;
}

bool devknx_macos_menu_installed(void) {
    NSMenu *main = [NSApp mainMenu];
    NSArray<NSString *> *titles = @[@"devknx", @"File", @"Edit", @"View",
                                   @"Operation", @"Window", @"Help"];
    if ([main numberOfItems] != (NSInteger)[titles count]) return false;
    for (NSUInteger index = 0; index < [titles count]; index++) {
        NSMenuItem *item = [main itemAtIndex:index];
        if (![[item title] isEqualToString:titles[index]] || ![item submenu]) return false;
    }
    return [[[[main itemAtIndex:0] submenu] itemWithTitle:@"About devknx"] action] == @selector(showAbout:);
}

static void add_item(NSMenu *menu, NSString *title, SEL selector, NSString *key,
                     id target, NSEventModifierFlags modifiers) {
    NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:title action:selector keyEquivalent:key];
    if (target) [item setTarget:target];
    if (modifiers != NSEventModifierFlagCommand) [item setKeyEquivalentModifierMask:modifiers];
    [menu addItem:item];
}

static NSMenu *add_menu(NSMenu *main, NSString *title) {
    NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:title action:nil keyEquivalent:@""];
    NSMenu *menu = [[NSMenu alloc] initWithTitle:title];
    [item setSubmenu:menu];
    [main addItem:item];
    return menu;
}

void devknx_init_macos_app(const char *version, const uint8_t *icon, size_t icon_len) {
    @autoreleasepool {
        if (version) app_version = [[NSString alloc] initWithUTF8String:version];
        [[NSProcessInfo processInfo] setProcessName:@"devknx"];
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        if (icon && icon_len) {
            app_icon = [[NSImage alloc] initWithData:[NSData dataWithBytes:icon length:icon_len]];
            if (app_icon) [app setApplicationIconImage:app_icon];
        }
        // The process owns the handler for the complete GUI lifetime.
        menu_handler = [DevknxMenuHandler new];

        NSMenu *main = [[NSMenu alloc] initWithTitle:@"devknx"];
        [app setMainMenu:main];
        NSMenu *application = add_menu(main, @"devknx");
        add_item(application, @"About devknx", @selector(showAbout:), @"", menu_handler, 0);
        add_item(application, @"Settings…", @selector(connectionSettings:), @",", menu_handler, NSEventModifierFlagCommand);
        [application addItem:[NSMenuItem separatorItem]];
        NSMenu *services = [[NSMenu alloc] initWithTitle:@"Services"];
        NSMenuItem *services_item = [[NSMenuItem alloc] initWithTitle:@"Services" action:nil keyEquivalent:@""];
        [services_item setSubmenu:services];
        [app setServicesMenu:services];
        [application addItem:services_item];
        [application addItem:[NSMenuItem separatorItem]];
        add_item(application, @"Hide devknx", @selector(hide:), @"h", nil, NSEventModifierFlagCommand);
        add_item(application, @"Hide Others", @selector(hideOtherApplications:), @"h", nil,
                 NSEventModifierFlagCommand | NSEventModifierFlagOption);
        add_item(application, @"Show All", @selector(unhideAllApplications:), @"", nil, 0);
        [application addItem:[NSMenuItem separatorItem]];
        add_item(application, @"Quit devknx", @selector(terminate:), @"q", nil, NSEventModifierFlagCommand);

        NSMenu *file = add_menu(main, @"File");
        add_item(file, @"Open Capture…", @selector(openDatabase:), @"o", menu_handler, NSEventModifierFlagCommand);
        add_item(file, @"Export CSV…", @selector(exportCsv:), @"e", menu_handler, NSEventModifierFlagCommand);
        [file addItem:[NSMenuItem separatorItem]];
        add_item(file, @"Close Window", @selector(performClose:), @"w", nil, NSEventModifierFlagCommand);

        NSMenu *edit = add_menu(main, @"Edit");
        add_item(edit, @"Undo", @selector(undo:), @"z", nil, NSEventModifierFlagCommand);
        add_item(edit, @"Redo", @selector(redo:), @"Z", nil, NSEventModifierFlagCommand | NSEventModifierFlagShift);
        [edit addItem:[NSMenuItem separatorItem]];
        add_item(edit, @"Cut", @selector(cut:), @"x", nil, NSEventModifierFlagCommand);
        add_item(edit, @"Copy", @selector(copy:), @"c", nil, NSEventModifierFlagCommand);
        add_item(edit, @"Paste", @selector(paste:), @"v", nil, NSEventModifierFlagCommand);
        add_item(edit, @"Select All", @selector(selectAll:), @"a", nil, NSEventModifierFlagCommand);

        NSMenu *view = add_menu(main, @"View");
        add_item(view, @"Discover Gateways", @selector(discover:), @"d", menu_handler, NSEventModifierFlagCommand);
        add_item(view, @"Filter Captures", @selector(focusFilter:), @"f", menu_handler, NSEventModifierFlagCommand);
        add_item(view, @"Toggle Full Screen", @selector(toggleFullScreen:), @"f", nil,
                 NSEventModifierFlagCommand | NSEventModifierFlagControl);

        NSMenu *operation = add_menu(main, @"Operation");
        add_item(operation, @"Connect or Disconnect", @selector(toggleConnection:), @"k", menu_handler, NSEventModifierFlagCommand);
        [operation addItem:[NSMenuItem separatorItem]];
        add_item(operation, @"Read Group Value…", @selector(readGroup:), @"r", menu_handler, NSEventModifierFlagCommand);
        add_item(operation, @"Prepare Group Write…", @selector(writeGroup:), @"p", menu_handler, NSEventModifierFlagCommand);

        NSMenu *window = add_menu(main, @"Window");
        [app setWindowsMenu:window];
        add_item(window, @"Minimize", @selector(performMiniaturize:), @"m", nil, NSEventModifierFlagCommand);
        add_item(window, @"Zoom", @selector(performZoom:), @"", nil, 0);
        add_item(window, @"Bring All to Front", @selector(arrangeInFront:), @"", nil, 0);

        NSMenu *help = add_menu(main, @"Help");
        [app setHelpMenu:help];
        add_item(help, @"devknx Documentation", @selector(openHelp:), @"?", menu_handler, NSEventModifierFlagCommand);
    }
}
