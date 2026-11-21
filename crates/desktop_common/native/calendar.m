#import <EventKit/EventKit.h>
#import <Foundation/Foundation.h>
#import <Network/Network.h>
#import <Security/Security.h>
#import <UserNotifications/UserNotifications.h>
#import <dispatch/dispatch.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <string.h>

static NSString *string(const char *utf8) {
    return [NSString stringWithUTF8String:utf8];
}

static char *json(id value, NSString *error) {
    NSDictionary *result = error ? @{@"error": error} : @{@"value": value ?: NSNull.null};
    NSData *bytes = [NSJSONSerialization dataWithJSONObject:result options:0 error:NULL];
    NSString *text = [[NSString alloc] initWithData:bytes encoding:NSUTF8StringEncoding];
    return text ? strdup(text.UTF8String) : NULL;
}

void fern_calendar_free(char *text) {
    free(text);
}

static BOOL calendarAccess(void) {
    EKAuthorizationStatus status = [EKEventStore authorizationStatusForEntityType:EKEntityTypeEvent];
    if (@available(macOS 14.0, *)) {
        return status == EKAuthorizationStatusFullAccess;
    }
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    return status == EKAuthorizationStatusAuthorized;
#pragma clang diagnostic pop
}

void fern_calendar_access(void (*completion)(int)) {
    @autoreleasepool {
        if (calendarAccess()) {
            completion(1);
            return;
        }
        if (![NSBundle.mainBundle objectForInfoDictionaryKey:@"NSCalendarsUsageDescription"]) {
            completion(-1);
            return;
        }
        EKEventStore *store = [[EKEventStore alloc] init];
        void (^response)(BOOL, NSError *) = ^(BOOL granted, NSError *error) {
            // Keep the store alive until EventKit completes the request.
            (void)store;
            completion(granted ? 1 : 0);
        };
        if (@available(macOS 14.0, *)) {
            // EventKit requires full permission for reading. Fern never calls its write APIs.
            [store requestFullAccessToEventsWithCompletion:response];
        } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            [store requestAccessToEntityType:EKEntityTypeEvent completion:response];
#pragma clang diagnostic pop
        }
    }
}

static BOOL iCloudCalendar(EKCalendar *calendar) {
    EKSource *source = calendar.source;
    // Public EventKit APIs expose the same CalDAV type for Google. Limit selection to
    // the system's iCloud source, rather than enumerating every CalDAV calendar.
    return calendar.type == EKCalendarTypeCalDAV && source.sourceType == EKSourceTypeCalDAV && source.title.length > 0 &&
        [source.title caseInsensitiveCompare:@"iCloud"] == NSOrderedSame;
}

static nw_path_monitor_t networkMonitor;
static atomic_int networkState;
static dispatch_semaphore_t firstNetworkUpdate;

void fern_calendar_monitor_network(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        firstNetworkUpdate = dispatch_semaphore_create(0);
        networkMonitor = nw_path_monitor_create();
        nw_path_monitor_set_queue(networkMonitor, dispatch_get_global_queue(QOS_CLASS_UTILITY, 0));
        nw_path_monitor_set_update_handler(networkMonitor, ^(nw_path_t path) {
            int current = nw_path_get_status(path) == nw_path_status_satisfied ? 1 : -1;
            if (atomic_exchange(&networkState, current) == 0) {
                dispatch_semaphore_signal(firstNetworkUpdate);
            }
        });
        nw_path_monitor_start(networkMonitor);
    });
}

int fern_calendar_connection(void) {
    return atomic_load(&networkState);
}

int fern_icloud_calendar_access(void) {
    return calendarAccess() ? 1 : 0;
}

static BOOL networkAvailable(void) {
    fern_calendar_monitor_network();
    if (atomic_load(&networkState) == 0) {
        dispatch_semaphore_wait(firstNetworkUpdate, dispatch_time(DISPATCH_TIME_NOW, 500 * NSEC_PER_MSEC));
    }
    return atomic_load(&networkState) > 0;
}

char *fern_icloud_calendars(void) {
    @autoreleasepool {
        if (!calendarAccess()) {
            return json(nil, @"iCloud calendar access is unavailable. Check macOS Calendar privacy settings.");
        }
        if (!networkAvailable()) {
            return json(nil, @"iCloud calendar availability is stale while this Mac is offline.");
        }
        @try {
            EKEventStore *store = [[EKEventStore alloc] init];
            NSMutableArray *result = [NSMutableArray array];
            for (EKCalendar *calendar in [store calendarsForEntityType:EKEntityTypeEvent]) {
                if (iCloudCalendar(calendar)) {
                    [result addObject:@{
                        @"source": calendar.source.sourceIdentifier,
                        @"source_name": calendar.source.title,
                        @"id": calendar.calendarIdentifier,
                        @"name": calendar.title
                    }];
                }
            }
            return json(result, nil);
        } @catch (NSException *exception) {
            return json(nil, @"Could not read iCloud calendars from EventKit.");
        }
    }
}

char *fern_icloud_events(const char *source, const char *identifier, double start, double end) {
    @autoreleasepool {
        if (!calendarAccess()) {
            return json(nil, @"iCloud calendar access is unavailable. Check macOS Calendar privacy settings.");
        }
        if (!networkAvailable()) {
            return json(nil, @"iCloud calendar availability is stale while this Mac is offline.");
        }
        @try {
            EKEventStore *store = [[EKEventStore alloc] init];
            EKCalendar *calendar = [store calendarWithIdentifier:string(identifier)];
            if (!calendar || !iCloudCalendar(calendar) || ![calendar.source.sourceIdentifier isEqualToString:string(source)]) {
                return json(nil, @"The selected iCloud calendar is unavailable.");
            }
            NSDate *from = [NSDate dateWithTimeIntervalSince1970:start];
            NSDate *to = [NSDate dateWithTimeIntervalSince1970:end];
            NSPredicate *predicate = [store predicateForEventsWithStartDate:from endDate:to calendars:@[calendar]];
            NSMutableArray *result = [NSMutableArray array];
            NSDateFormatter *date = [[NSDateFormatter alloc] init];
            date.locale = [NSLocale localeWithLocaleIdentifier:@"en_US_POSIX"];
            date.calendar = [[NSCalendar alloc] initWithCalendarIdentifier:NSCalendarIdentifierGregorian];
            date.timeZone = NSTimeZone.localTimeZone;
            date.dateFormat = @"yyyy-MM-dd";
            NSISO8601DateFormatter *instant = [[NSISO8601DateFormatter alloc] init];
            instant.formatOptions = NSISO8601DateFormatWithInternetDateTime | NSISO8601DateFormatWithFractionalSeconds;
            NSArray<EKEvent *> *matches = [store eventsMatchingPredicate:predicate];
            if (!matches) {
                return json(nil, @"EventKit did not return calendar availability.");
            }
            for (EKEvent *event in matches) {
                if (event.status == EKEventStatusCanceled) {
                    continue;
                }
                NSString *identifier = event.calendarItemExternalIdentifier ?: event.calendarItemIdentifier;
                if (!identifier || !event.startDate || !event.endDate) {
                    return json(nil, @"EventKit returned an incomplete event.");
                }
                if (event.occurrenceDate) {
                    NSString *original = event.isAllDay ? [date stringFromDate:event.occurrenceDate] :
                        [instant stringFromDate:event.occurrenceDate];
                    identifier = [NSString stringWithFormat:@"%@/%@", identifier, original];
                }
                BOOL busy = event.availability != EKEventAvailabilityFree;
                for (EKParticipant *attendee in event.attendees) {
                    if (attendee.isCurrentUser && attendee.participantStatus == EKParticipantStatusDeclined) {
                        busy = NO;
                    }
                }
                NSDictionary *span;
                if (event.isAllDay) {
                    span = @{@"kind": @"all_day", @"start": [date stringFromDate:event.startDate],
                             @"end": [date stringFromDate:event.endDate]};
                } else {
                    NSDate *end = event.endDate;
                    if ([event.startDate compare:end] != NSOrderedAscending) {
                        busy = NO;
                        end = [event.startDate dateByAddingTimeInterval:0.001];
                    }
                    span = @{@"kind": @"timed", @"slot": @{
                        @"start": [instant stringFromDate:event.startDate], @"end": [instant stringFromDate:end]
                    }};
                }
                [result addObject:@{
                    @"id": identifier, @"title": event.title ?: @"", @"location": event.location ?: @"",
                    @"notes": event.notes ?: @"", @"span": span, @"busy": @(busy)
                }];
            }
            return json(result, nil);
        } @catch (NSException *exception) {
            return json(nil, @"Could not read events from the iCloud calendar.");
        }
    }
}

static NSDictionary *keychainQuery(const char *profile) {
    return @{
        (__bridge id)kSecClass: (__bridge id)kSecClassGenericPassword,
        (__bridge id)kSecAttrService: @"io.github.atabits.fern.google-calendar",
        (__bridge id)kSecAttrAccount: string(profile)
    };
}

static NSString *keychainError(OSStatus status) {
    return [NSString stringWithFormat:@"Could not access Fern's Google Calendar Keychain entry (%d).", (int)status];
}

char *fern_google_keychain_get(const char *profile) {
    @autoreleasepool {
        NSMutableDictionary *query = [keychainQuery(profile) mutableCopy];
        query[(__bridge id)kSecReturnData] = @YES;
        query[(__bridge id)kSecMatchLimit] = (__bridge id)kSecMatchLimitOne;
        CFTypeRef result = NULL;
        OSStatus status = SecItemCopyMatching((__bridge CFDictionaryRef)query, &result);
        if (status == errSecItemNotFound) {
            return json(nil, nil);
        }
        if (status != errSecSuccess) {
            return json(nil, keychainError(status));
        }
        NSData *bytes = CFBridgingRelease(result);
        return json([[NSString alloc] initWithData:bytes encoding:NSUTF8StringEncoding], nil);
    }
}

char *fern_google_keychain_set(const char *profile, const char *data) {
    @autoreleasepool {
        NSDictionary *query = keychainQuery(profile);
        NSData *bytes = [string(data) dataUsingEncoding:NSUTF8StringEncoding];
        NSDictionary *update = @{(__bridge id)kSecValueData: bytes};
        OSStatus status = SecItemUpdate((__bridge CFDictionaryRef)query, (__bridge CFDictionaryRef)update);
        if (status == errSecItemNotFound) {
            NSMutableDictionary *item = [query mutableCopy];
            [item addEntriesFromDictionary:update];
            status = SecItemAdd((__bridge CFDictionaryRef)item, NULL);
            if (status == errSecDuplicateItem) {
                status = SecItemUpdate((__bridge CFDictionaryRef)query, (__bridge CFDictionaryRef)update);
            }
        }
        return status == errSecSuccess ? json(nil, nil) : json(nil, keychainError(status));
    }
}

static NSString *const followUpCategory = @"fern-work-follow-up";
static NSString *const followUpPrefix = @"fern-work-follow-up/";
static UNUserNotificationCenter *notificationCenter;
static NSMutableArray<NSDictionary *> *notificationActions;
static int notificationPermission;
static NSUInteger pruneGeneration;

@interface FernFollowUpDelegate : NSObject <UNUserNotificationCenterDelegate>
@end

@implementation FernFollowUpDelegate
- (void)userNotificationCenter:(UNUserNotificationCenter *)center
       willPresentNotification:(UNNotification *)notification
         withCompletionHandler:(void (^)(UNNotificationPresentationOptions))completion {
    completion(UNNotificationPresentationOptionBanner | UNNotificationPresentationOptionList | UNNotificationPresentationOptionSound);
}

- (void)userNotificationCenter:(UNUserNotificationCenter *)center
didReceiveNotificationResponse:(UNNotificationResponse *)response
         withCompletionHandler:(void (^)(void))completion {
    UNNotificationContent *content = response.notification.request.content;
    NSString *action = response.actionIdentifier;
    if ([content.categoryIdentifier isEqualToString:followUpCategory]
        && content.userInfo[@"task"] && content.userInfo[@"notice"]
        && ![action isEqualToString:UNNotificationDismissActionIdentifier]) {
        NSString *minutes = @"";
        if ([response isKindOfClass:UNTextInputNotificationResponse.class]) {
            minutes = ((UNTextInputNotificationResponse *)response).userText;
        }
        @synchronized (notificationActions) {
            [notificationActions addObject:@{
                @"task": content.userInfo[@"task"], @"notice": content.userInfo[@"notice"],
                @"action": [action isEqualToString:UNNotificationDefaultActionIdentifier] ? @"open" : action,
                @"minutes": minutes
            }];
        }
    }
    completion();
}
@end

static FernFollowUpDelegate *notificationDelegate;

void fern_follow_up_permissions(void) {
    @autoreleasepool {
        [notificationCenter getNotificationSettingsWithCompletionHandler:^(UNNotificationSettings *settings) {
            @synchronized (notificationActions) {
                if (settings.authorizationStatus == UNAuthorizationStatusAuthorized || settings.authorizationStatus == UNAuthorizationStatusProvisional) {
                    notificationPermission = 1;
                } else if (settings.authorizationStatus == UNAuthorizationStatusDenied) {
                    notificationPermission = -1;
                } else {
                    notificationPermission = 0;
                }
            }
        }];
    }
}

void fern_follow_up_configure(const char *done, const char *snooze, const char *minutes, const char *reschedule) {
    @autoreleasepool {
        if (!NSBundle.mainBundle.bundleIdentifier) {
            return;
        }
        if (!notificationCenter) {
            notificationActions = [NSMutableArray array];
            notificationDelegate = [[FernFollowUpDelegate alloc] init];
            notificationCenter = UNUserNotificationCenter.currentNotificationCenter;
            notificationCenter.delegate = notificationDelegate;
        }
        UNNotificationAction *finish = [UNNotificationAction actionWithIdentifier:@"done" title:string(done) options:0];
        UNNotificationAction *later = [UNTextInputNotificationAction actionWithIdentifier:@"snooze"
            title:string(snooze) options:0 textInputButtonTitle:string(snooze) textInputPlaceholder:string(minutes)];
        UNNotificationAction *move = [UNNotificationAction actionWithIdentifier:@"reschedule"
            title:string(reschedule) options:UNNotificationActionOptionForeground];
        UNNotificationCategory *category = [UNNotificationCategory categoryWithIdentifier:followUpCategory
            actions:@[finish, later, move] intentIdentifiers:@[] options:0];
        [notificationCenter setNotificationCategories:[NSSet setWithObject:category]];
        fern_follow_up_permissions();
    }
}

void fern_follow_up_request(void) {
    @autoreleasepool {
        [notificationCenter requestAuthorizationWithOptions:(UNAuthorizationOptionAlert | UNAuthorizationOptionSound)
            completionHandler:^(BOOL granted, NSError *error) { fern_follow_up_permissions(); }];
    }
}

int fern_follow_up_permission(void) {
    if (!notificationCenter) {
        return -2;
    }
    @synchronized (notificationActions) {
        return notificationPermission;
    }
}

char *fern_follow_up_actions(void) {
    @autoreleasepool {
        @synchronized (notificationActions) {
            NSArray *actions = [notificationActions copy] ?: @[];
            [notificationActions removeAllObjects];
            return json(actions, nil);
        }
    }
}

void fern_follow_up_show(const char *task, const char *notice, const char *title, const char *body) {
    @autoreleasepool {
        NSString *taskID = string(task);
        NSString *identifier = [NSString stringWithFormat:@"%@%@/%@", followUpPrefix, taskID, string(notice)];
        UNMutableNotificationContent *content = [[UNMutableNotificationContent alloc] init];
        content.title = string(title);
        content.body = string(body);
        content.sound = UNNotificationSound.defaultSound;
        content.categoryIdentifier = followUpCategory;
        content.userInfo = @{@"task": taskID, @"notice": string(notice)};
        UNNotificationRequest *request = [UNNotificationRequest requestWithIdentifier:identifier content:content trigger:nil];
        [notificationCenter addNotificationRequest:request withCompletionHandler:nil];
    }
}

void fern_follow_up_prune(const char *valid) {
    @autoreleasepool {
        if (!notificationCenter) {
            return;
        }
        NSData *bytes = [string(valid) dataUsingEncoding:NSUTF8StringEncoding];
        NSDictionary *active = [NSJSONSerialization JSONObjectWithData:bytes options:0 error:NULL];
        NSUInteger generation;
        @synchronized (notificationActions) {
            generation = ++pruneGeneration;
        }
        [notificationCenter getDeliveredNotificationsWithCompletionHandler:^(NSArray<UNNotification *> *notifications) {
            @synchronized (notificationActions) {
                if (generation != pruneGeneration) {
                    return;
                }
                NSMutableArray *remove = [NSMutableArray array];
                for (UNNotification *notification in notifications) {
                    UNNotificationRequest *request = notification.request;
                    if (![request.identifier hasPrefix:followUpPrefix]) {
                        continue;
                    }
                    NSDictionary *info = request.content.userInfo;
                    id token = info[@"task"] ? active[info[@"task"]] : nil;
                    if (![token isEqual:info[@"notice"]]) {
                        [remove addObject:request.identifier];
                    }
                }
                [notificationCenter removeDeliveredNotificationsWithIdentifiers:remove];
            }
        }];
        [notificationCenter getPendingNotificationRequestsWithCompletionHandler:^(NSArray<UNNotificationRequest *> *requests) {
            @synchronized (notificationActions) {
                if (generation != pruneGeneration) {
                    return;
                }
                NSMutableArray *remove = [NSMutableArray array];
                for (UNNotificationRequest *request in requests) {
                    NSDictionary *info = request.content.userInfo;
                    if ([request.identifier hasPrefix:followUpPrefix]
                        && info[@"task"] && ![active[info[@"task"]] isEqual:info[@"notice"]]) {
                        [remove addObject:request.identifier];
                    }
                }
                [notificationCenter removePendingNotificationRequestsWithIdentifiers:remove];
            }
        }];
    }
}
