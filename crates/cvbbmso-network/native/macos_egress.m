#import <Foundation/Foundation.h>
#import <Security/Security.h>
#import <ServiceManagement/ServiceManagement.h>
#include <mach-o/dyld.h>
#include <sys/acl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#include <xpc/xpc.h>

static NSString *const AppPath = @"/Applications/CVBBMSO.app";
static NSString *const ServicePath = @"/Applications/CVBBMSO.app/Contents/MacOS/cvbbmso-egress-service";
static NSString *const AppID = @"edu.buaa.accoding.cvbbmso";
static NSString *const ServiceID = @"edu.buaa.accoding.cvbbmso.egress";
static NSString *const PlistName = @"edu.buaa.accoding.cvbbmso.egress.plist";

// The values and operation numbers are shared with egress/managed.rs.
enum { Ok, Unsupported, Untrusted, ApprovalRequired, Unavailable, BadPF, Busy, Platform };
enum { Install = 1, Verify, Shutdown, Disconnected };
typedef int (*Handler)(void *, uint64_t, uint64_t);

static NSString *requirement(NSString *identifier, const char *team) {
    if (strlen(team) != 10 || strspn(team, "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789") != 10) return nil;
    NSString *base = [NSString stringWithFormat:
        @"anchor apple generic and identifier \"%@\" "
        "and certificate 1[field.1.2.840.113635.100.6.2.6] exists "
        "and certificate leaf[field.1.2.840.113635.100.6.1.13] exists "
        "and certificate leaf[subject.OU] = \"%s\" "
        "and entitlement[\"com.apple.security.get-task-allow\"] absent "
        "and entitlement[\"com.apple.security.cs.disable-library-validation\"] absent "
        "and entitlement[\"com.apple.security.cs.allow-dyld-environment-variables\"] absent",
        identifier, team];
    return [identifier isEqualToString:AppID] ?
        [base stringByAppendingString:@" and info[CVBBMSOPrivilegedServiceVersion] = \"1\""] : base;
}

static BOOL checkStatic(NSString *path, NSString *text, BOOL nested) {
    if (!text) return NO;
    SecRequirementRef requirement = NULL;
    SecStaticCodeRef code = NULL;
    OSStatus status = SecRequirementCreateWithString((__bridge CFStringRef)text, kSecCSDefaultFlags, &requirement);
    if (status == errSecSuccess) {
        status = SecStaticCodeCreateWithPath((__bridge CFURLRef)[NSURL fileURLWithPath:path], kSecCSDefaultFlags, &code);
    }
    if (status == errSecSuccess) {
        SecCSFlags flags = kSecCSStrictValidate | kSecCSCheckAllArchitectures;
        if (nested) flags |= kSecCSCheckNestedCode;
        status = SecStaticCodeCheckValidity(code, flags, requirement);
    }
    if (code) CFRelease(code);
    if (requirement) CFRelease(requirement);
    return status == errSecSuccess;
}

static BOOL checkSelf(NSString *text) {
    if (!text) return NO;
    SecRequirementRef requirement = NULL;
    SecCodeRef code = NULL;
    OSStatus status = SecRequirementCreateWithString((__bridge CFStringRef)text, kSecCSDefaultFlags, &requirement);
    if (status == errSecSuccess) status = SecCodeCopySelf(kSecCSDefaultFlags, &code);
    if (status == errSecSuccess) status = SecCodeCheckValidity(code, kSecCSDefaultFlags, requirement);
    if (code) CFRelease(code);
    if (requirement) CFRelease(requirement);
    return status == errSecSuccess;
}

static BOOL applicationsDirectory(void) {
    struct stat info;
    if (lstat("/Applications", &info) || !S_ISDIR(info.st_mode) || info.st_uid != 0 ||
        (info.st_mode & S_IWOTH) || ((info.st_mode & S_IWGRP) && info.st_gid != 80)) return NO;
    acl_t acl = acl_get_link_np("/Applications", ACL_TYPE_EXTENDED);
    if (!acl) return errno == ENOENT || errno == ENOATTR;
    acl_entry_t entry;
    int hasEntry = acl_get_entry(acl, ACL_FIRST_ENTRY, &entry);
    int error = errno;
    acl_free(acl);
    return hasEntry == -1 && error == EINVAL;
}

static BOOL validInstallation(NSString *executable, const char *team) {
    struct statfs mountInfo;
    char actual[PATH_MAX], normalized[PATH_MAX];
    uint32_t size = sizeof(actual);
    return executable && applicationsDirectory() &&
        !statfs(AppPath.fileSystemRepresentation, &mountInfo) && !(mountInfo.f_flags & MNT_RDONLY) &&
        !_NSGetExecutablePath(actual, &size) && realpath(actual, normalized) &&
        !strcmp(normalized, executable.fileSystemRepresentation) &&
        [executable hasPrefix:[AppPath stringByAppendingString:@"/Contents/MacOS/"]] &&
        checkStatic(AppPath, requirement(AppID, team), YES) &&
        checkStatic(ServicePath, requirement(ServiceID, team), NO);
}

static BOOL validSpawnConstraint(const char *team) {
    NSString *directory = [AppPath stringByAppendingPathComponent:@"Contents/Library/LaunchDaemons"];
    NSString *path = [directory stringByAppendingPathComponent:PlistName];
    NSDictionary *plist = [NSDictionary dictionaryWithContentsOfFile:path];
    NSDictionary *constraint = plist[@"SpawnConstraint"];
    return [constraint isKindOfClass:NSDictionary.class] &&
        [constraint[@"team-identifier"] isEqual:@(team)] &&
        [constraint[@"signing-identifier"] isEqual:ServiceID] &&
        [constraint[@"validation-category"] isEqual:@6];
}

int cvbbmso_register_service(const char *team) {
    @autoreleasepool {
        if (@available(macOS 14.0, *)) {
            NSBundle *bundle = NSBundle.mainBundle;
            if (geteuid() == 0 || ![bundle.bundlePath isEqualToString:AppPath] ||
                !validInstallation(bundle.executablePath, team) || !validSpawnConstraint(team) ||
                !checkSelf(requirement(AppID, team))) return Untrusted;
            SMAppService *service = [SMAppService daemonServiceWithPlistName:PlistName];
            if (service.status == SMAppServiceStatusNotRegistered) [service registerAndReturnError:NULL];
            switch (service.status) {
                case SMAppServiceStatusEnabled: return Ok;
                case SMAppServiceStatusRequiresApproval: return ApprovalRequired;
                default: return Unavailable;
            }
        }
        return Unsupported;
    }
}

int cvbbmso_approval_settings(void) {
    if (@available(macOS 14.0, *)) {
        dispatch_async(dispatch_get_main_queue(), ^{ [SMAppService openSystemSettingsLoginItems]; });
        return Ok;
    }
    return Unsupported;
}

bool cvbbmso_validate_recovery(const char *path, const char *team, bool liveCopy) {
    @autoreleasepool {
        NSString *text = requirement(ServiceID, team);
        if (geteuid() != 0 || !checkStatic(@(path), text, NO)) return false;
        if (!liveCopy) return true;
        SecStaticCodeRef candidate = NULL;
        CFDictionaryRef information = NULL;
        OSStatus status = SecStaticCodeCreateWithPath((__bridge CFURLRef)[NSURL fileURLWithPath:@(path)], kSecCSDefaultFlags, &candidate);
        if (status == errSecSuccess) status = SecCodeCopySigningInformation(candidate, kSecCSDefaultFlags, &information);
        NSData *hash = status == errSecSuccess && information ?
            (__bridge NSData *)CFDictionaryGetValue(information, kSecCodeInfoUnique) : nil;
        NSMutableString *requireLiveHash = [text mutableCopy];
        [requireLiveHash appendString:@" and cdhash H\""];
        for (NSUInteger index = 0; index < hash.length; index++) {
            [requireLiveHash appendFormat:@"%02x", ((const unsigned char *)hash.bytes)[index]];
        }
        [requireLiveHash appendString:@"\""];
        BOOL matches = hash.length == 20 && checkSelf(requireLiveHash);
        if (information) CFRelease(information);
        if (candidate) CFRelease(candidate);
        return matches;
    }
}

bool cvbbmso_validate_service(const char *team, bool bundled) {
    @autoreleasepool {
        if (@available(macOS 14.0, *)) {
            return geteuid() == 0 && checkSelf(requirement(ServiceID, team)) &&
                (!bundled || (validInstallation(ServicePath, team) && validSpawnConstraint(team)));
        }
        return false;
    }
}

static void rejectInactiveConnection(xpc_connection_t connection) {
    xpc_connection_set_event_handler(connection, ^(__unused xpc_object_t event) {});
    xpc_connection_cancel(connection);
    xpc_connection_activate(connection);
}

void *cvbbmso_connect(const char *team) {
    @autoreleasepool {
        if (@available(macOS 14.0, *)) {
            NSString *text = requirement(ServiceID, team);
            if (!text) return NULL;
            xpc_connection_t peer = xpc_connection_create_mach_service(ServiceID.UTF8String, NULL, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED);
            if (!peer) return NULL;
            if (xpc_connection_set_peer_code_signing_requirement(peer, text.UTF8String)) {
                rejectInactiveConnection(peer);
                return NULL;
            }
            __weak xpc_connection_t connection = peer;
            xpc_connection_set_event_handler(peer, ^(xpc_object_t event) {
                xpc_connection_t active = connection;
                if (active && xpc_get_type(event) == XPC_TYPE_ERROR) xpc_connection_cancel(active);
            });
            xpc_connection_activate(peer);
            return (__bridge_retained void *)peer;
        }
        return NULL;
    }
}

void cvbbmso_disconnect(void *handle) {
    @autoreleasepool {
        xpc_connection_t peer = (__bridge_transfer xpc_connection_t)handle;
        xpc_connection_cancel(peer);
    }
}

int cvbbmso_request(void *handle, uint64_t operation) {
    @autoreleasepool {
        xpc_connection_t peer = (__bridge xpc_connection_t)handle;
        xpc_object_t request = xpc_dictionary_create(NULL, NULL, 0);
        xpc_dictionary_set_uint64(request, "version", 1);
        xpc_dictionary_set_uint64(request, "operation", operation);
        dispatch_semaphore_t answered = dispatch_semaphore_create(0);
        __block int status = Unavailable;
        xpc_connection_send_message_with_reply(peer, request, dispatch_get_global_queue(QOS_CLASS_UTILITY, 0), ^(xpc_object_t reply) {
            if (xpc_get_type(reply) == XPC_TYPE_DICTIONARY && xpc_dictionary_get_count(reply) == 1 &&
                xpc_dictionary_get_value(reply, "result") &&
                xpc_get_type(xpc_dictionary_get_value(reply, "result")) == XPC_TYPE_INT64) {
                int64_t result = xpc_dictionary_get_int64(reply, "result");
                if (result >= 0 && result <= 10) status = (int)result;
            }
            dispatch_semaphore_signal(answered);
        });
        if (dispatch_semaphore_wait(answered, dispatch_time(DISPATCH_TIME_NOW, 45 * NSEC_PER_SEC))) {
            xpc_connection_cancel(peer);
            return Unavailable;
        }
        return status;
    }
}

int cvbbmso_listen(const char *team, void *context, Handler handle) {
    @autoreleasepool {
        if (@available(macOS 14.0, *)) {
            NSString *text = requirement(AppID, team);
            if (!text) return Untrusted;
            dispatch_queue_t queue = dispatch_queue_create("edu.buaa.accoding.cvbbmso.egress", DISPATCH_QUEUE_SERIAL);
            __attribute__((objc_precise_lifetime)) xpc_connection_t listener =
                xpc_connection_create_mach_service(ServiceID.UTF8String, queue, XPC_CONNECTION_MACH_SERVICE_LISTENER);
            if (!listener) return Unavailable;
            if (xpc_connection_set_peer_code_signing_requirement(listener, text.UTF8String)) {
                rejectInactiveConnection(listener);
                return Unavailable;
            }
            __block uint64_t next = 0;
            NSMutableDictionary<NSNumber *, id> *peers = [NSMutableDictionary dictionary];
            xpc_connection_set_event_handler(listener, ^(xpc_object_t incoming) {
                if (xpc_get_type(incoming) == XPC_TYPE_ERROR) _exit(EXIT_FAILURE);
                if (xpc_get_type(incoming) != XPC_TYPE_CONNECTION) return;
                xpc_connection_t peer = (xpc_connection_t)incoming;
                if (xpc_connection_set_peer_code_signing_requirement(peer, text.UTF8String)) {
                    rejectInactiveConnection(peer);
                    return;
                }
                uint64_t session = ++next;
                NSNumber *key = @(session);
                peers[key] = peer;
                __block BOOL closed = NO;
                xpc_connection_set_target_queue(peer, queue);
                __weak xpc_connection_t connection = peer;
                xpc_connection_set_event_handler(peer, ^(xpc_object_t event) {
                    xpc_connection_t active = connection;
                    if (closed || !active) return;
                    if (xpc_get_type(event) == XPC_TYPE_ERROR) {
                        closed = YES;
                        handle(context, session, Disconnected);
                        xpc_connection_cancel(active);
                        [peers removeObjectForKey:key];
                        return;
                    }
                    if (xpc_get_type(event) != XPC_TYPE_DICTIONARY) return;
                    xpc_object_t reply = xpc_dictionary_create_reply(event);
                    if (!reply) return;
                    // All requests must be synchronous and from a GUI user.
                    uint64_t operation = xpc_dictionary_get_uint64(event, "operation");
                    int status = Platform;
                    if (xpc_dictionary_get_count(event) == 2 && xpc_dictionary_get_uint64(event, "version") == 1 &&
                        operation >= Install && operation <= Shutdown && xpc_connection_get_euid(active) >= 501 &&
                        xpc_connection_get_euid(active) != (uid_t)-1) status = handle(context, session, operation);
                    xpc_dictionary_set_int64(reply, "result", status);
                    xpc_connection_send_message(active, reply);
                });
                xpc_connection_activate(peer);
            });
            xpc_connection_activate(listener);
            dispatch_semaphore_t lifetime = dispatch_semaphore_create(0);
            dispatch_semaphore_wait(lifetime, DISPATCH_TIME_FOREVER);
            return Unavailable;
        }
        return Unsupported;
    }
}
