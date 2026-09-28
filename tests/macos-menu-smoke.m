// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

#import <AppKit/AppKit.h>
#import <stdbool.h>
#import <stdint.h>
#import <stdio.h>

extern void devknx_init_macos_app(const char *version, const uint8_t *icon, size_t icon_len);
extern bool devknx_take_menu_action(uint32_t action);
extern bool devknx_macos_menu_installed(void);

static int failures = 0;

static void expect(bool condition, const char *description) {
    if (!condition) {
        fprintf(stderr, "menu smoke failed: %s\n", description);
        failures++;
    }
}

static NSMenu *submenu(NSString *title) {
    NSMenuItem *item = [[NSApp mainMenu] itemWithTitle:title];
    return [item submenu];
}

static void check_action(NSString *menu_title, NSString *item_title,
                         NSString *key, uint32_t flag) {
    NSMenuItem *item = [submenu(menu_title) itemWithTitle:item_title];
    expect(item != nil, [item_title UTF8String]);
    if (!item) return;
    expect([[item keyEquivalent] isEqualToString:key], "key equivalent");
    expect([NSApp sendAction:[item action] to:[item target] from:item], "AppKit dispatches action");
    expect(devknx_take_menu_action(flag), "native action reaches Rust bridge");
    expect(!devknx_take_menu_action(flag), "native action is consumed once");
}

int main(void) {
    @autoreleasepool {
        devknx_init_macos_app("0.1.0", NULL, 0);
        expect(devknx_macos_menu_installed(), "menu is installed in AppKit");
        expect([[NSApp mainMenu] numberOfItems] == 7, "seven native menus");
        for (NSString *title in @[@"devknx", @"File", @"Edit", @"View",
                                 @"Operation", @"Window", @"Help"]) {
            expect(submenu(title) != nil, [title UTF8String]);
        }
        expect([submenu(@"devknx") itemWithTitle:@"About devknx"] != nil, "About panel item");
        expect([[submenu(@"devknx") itemWithTitle:@"Quit devknx"].keyEquivalent isEqualToString:@"q"], "Quit shortcut");
        for (NSString *title in @[@"Undo", @"Redo", @"Cut", @"Copy", @"Paste", @"Select All"]) {
            expect([submenu(@"Edit") itemWithTitle:title] != nil, [title UTF8String]);
        }
        check_action(@"File", @"Open Capture…", @"o", 1);
        check_action(@"File", @"Export CSV…", @"e", 2);
        check_action(@"View", @"Discover Gateways", @"d", 4);
        check_action(@"View", @"Filter Captures", @"f", 8);
        check_action(@"Operation", @"Read Group Value…", @"r", 16);
        check_action(@"Operation", @"Prepare Group Write…", @"p", 32);
        if (failures == 0) puts("native menu and action bridge: ok");
        return failures == 0 ? 0 : 1;
    }
}
