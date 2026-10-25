// Entry point: hands the main thread to the Rust staticlib, which runs the
// winit/eframe event loop and does not return in normal operation.
#import <UIKit/UIKit.h>
#import <objc/runtime.h>

extern void asteria_spike_main(void);

// iOS 27 devices refuse to launch apps built with the iOS 27 SDK that have not
// adopted the UIScene lifecycle (the simulator only logs it). winit 0.30 creates
// its UIWindow without a scene, so Info.plist declares a scene and this delegate
// attaches winit's window to it once the scene connects.

static __weak UIWindow *pendingWindow;
static __weak UIWindowScene *connectedScene;
static void (*originalMakeKeyAndVisible)(id, SEL);

static void asteriaMakeKeyAndVisible(UIWindow *self, SEL _cmd) {
    if (self.windowScene == nil) {
        if (connectedScene != nil) {
            self.windowScene = connectedScene;
        } else {
            pendingWindow = self;
        }
    }
    originalMakeKeyAndVisible(self, _cmd);
}

@interface AsteriaSceneDelegate : UIResponder <UIWindowSceneDelegate>
@property(nonatomic, strong) UIWindow *window;
@end

@implementation AsteriaSceneDelegate
- (void)scene:(UIScene *)scene
    willConnectToSession:(UISceneSession *)session
                 options:(UISceneConnectionOptions *)connectionOptions {
    if (![scene isKindOfClass:[UIWindowScene class]]) return;
    connectedScene = (UIWindowScene *)scene;
    UIWindow *window = pendingWindow;
    if (window != nil) {
        pendingWindow = nil;
        window.windowScene = connectedScene;
        self.window = window;
        originalMakeKeyAndVisible(window, @selector(makeKeyAndVisible));
    }
}
@end

int main(int argc, char *argv[]) {
    Method m = class_getInstanceMethod([UIWindow class], @selector(makeKeyAndVisible));
    originalMakeKeyAndVisible = (void (*)(id, SEL))method_setImplementation(m, (IMP)asteriaMakeKeyAndVisible);
    asteria_spike_main();
    return 0;
}
