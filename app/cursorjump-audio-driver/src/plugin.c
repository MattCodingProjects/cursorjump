/*
 * CursorJumpAudio.driver: a Core Audio server plug-in (HAL plug-in) with two
 * virtual devices on one shared clock.
 *
 *   "CursorJump Microphone"  com.cursorjump.audio.mic   input, 1 ch, visible
 *   "CursorJump Mic Feed"    com.cursorjump.audio.feed  output, 1 ch, hidden
 *
 * The CursorJump app plays the PC's microphone into the hidden Mic Feed (it
 * opens it by UID). The feed's WriteMix stores each frame in a ring at its
 * output sample time; the Microphone's ReadInput takes frames from the ring
 * at its input sample time. Both devices run on one clock (one anchor host
 * time, one zero time stamp sequence), so the two sample times are on the
 * same time base and the ring needs no offset. Whatever was never fed reads
 * as silence.
 *
 * The app starts and stops the PC's mic by listening for
 * kAudioDevicePropertyDeviceIsRunningSomewhere on the Microphone: it is true
 * while any client has IO started on it.
 *
 * Written from Apple's public AudioServerPlugIn.h and AudioHardwareBase.h.
 * Format: 48 kHz, float32, mono, packed; nothing else is offered.
 *
 * Realtime rule: DoIOOperation, BeginIOOperation, EndIOOperation and
 * GetZeroTimeStamp take no locks, allocate nothing and log nothing. Other
 * calls may take state_lock.
 */
#include <CoreAudio/AudioHardware.h>
#include <CoreAudio/AudioServerPlugIn.h>
#include <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <string.h>

#include "ring.h"

/* ---- Constants -------------------------------------------------------- */

/*
 * The factory UUID, 415040BD-B44A-418C-8388-2B1BD52D3D5A. Info.plist maps it
 * to CursorJumpAudio_Create (CFPlugInFactories) and lists it under
 * kAudioServerPlugInTypeUUID (CFPlugInTypes); CFPlugIn matches it from
 * there, so the code only needs it for reference. Never change it.
 */
#define CJ_FACTORY_UUID_STRING "415040BD-B44A-418C-8388-2B1BD52D3D5A"

#define CJ_SAMPLE_RATE 48000.0
#define CJ_CHANNELS 1
#define CJ_BYTES_PER_FRAME (sizeof(Float32) * CJ_CHANNELS)
/* Frames between successive zero time stamps; one second. */
#define CJ_ZERO_TIMESTAMP_PERIOD 48000u
#define CJ_MIN_BUFFER_FRAMES 16.0
#define CJ_MAX_BUFFER_FRAMES 4096.0
/* Both devices report this clock domain: they share one clock. */
#define CJ_CLOCK_DOMAIN 0x434A4155u /* 'CJAU' */

enum {
    kObjectID_PlugIn = kAudioObjectPlugInObject, /* 1 */
    kObjectID_Mic = 2,
    kObjectID_MicStream = 3,
    kObjectID_Feed = 4,
    kObjectID_FeedStream = 5,
};

#define CJ_MANUFACTURER CFSTR("CursorJump")
#define CJ_MODEL_UID CFSTR("com.cursorjump.audio.model")
#define CJ_PLUGIN_BUNDLE_ID CFSTR("com.cursorjump.audio")

/* ---- Object model ----------------------------------------------------- */

typedef struct {
    AudioObjectID id;
    AudioObjectID stream_id;
    CFStringRef name;
    CFStringRef uid;
    bool is_input;
    bool can_be_default;
    bool is_hidden;
    /* Guarded by state_lock. */
    UInt32 io_count;
    UInt32 stream_is_active;
} cj_device;

static cj_device devices[] = {
    {
        .id = kObjectID_Mic,
        .stream_id = kObjectID_MicStream,
        .name = CFSTR("CursorJump Microphone"),
        .uid = CFSTR("com.cursorjump.audio.mic"),
        .is_input = true,
        .can_be_default = true,
        .is_hidden = false,
        .stream_is_active = 1,
    },
    {
        .id = kObjectID_Feed,
        .stream_id = kObjectID_FeedStream,
        .name = CFSTR("CursorJump Mic Feed"),
        .uid = CFSTR("com.cursorjump.audio.feed"),
        .is_input = false,
        .can_be_default = false,
        .is_hidden = true,
        .stream_is_active = 1,
    },
};
#define CJ_DEVICE_COUNT ((UInt32)(sizeof devices / sizeof devices[0]))

static cj_device *device_by_id(AudioObjectID id) {
    for (UInt32 i = 0; i < CJ_DEVICE_COUNT; i++)
        if (devices[i].id == id) return &devices[i];
    return NULL;
}

static cj_device *device_by_stream(AudioObjectID id) {
    for (UInt32 i = 0; i < CJ_DEVICE_COUNT; i++)
        if (devices[i].stream_id == id) return &devices[i];
    return NULL;
}

/* ---- Shared state ----------------------------------------------------- */

static pthread_mutex_t state_lock = PTHREAD_MUTEX_INITIALIZER;
static AudioServerPlugInHostRef host;
static dispatch_queue_t notify_queue;
static _Atomic ULONG ref_count = 0;

/* The shared clock. anchor_host_time is 0 while neither device runs. */
static Float64 host_ticks_per_frame;
static _Atomic UInt64 anchor_host_time = 0;

/* PC mic audio: written by the feed, read by the microphone. */
static cj_ring mic_ring;

static AudioStreamBasicDescription stream_format(void) {
    AudioStreamBasicDescription f = {
        .mSampleRate = CJ_SAMPLE_RATE,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian |
                        kAudioFormatFlagIsPacked,
        .mBytesPerPacket = CJ_BYTES_PER_FRAME,
        .mFramesPerPacket = 1,
        .mBytesPerFrame = CJ_BYTES_PER_FRAME,
        .mChannelsPerFrame = CJ_CHANNELS,
        .mBitsPerChannel = 32,
    };
    return f;
}

static bool is_our_format(const AudioStreamBasicDescription *f) {
    AudioStreamBasicDescription ours = stream_format();
    return f->mSampleRate == ours.mSampleRate && f->mFormatID == ours.mFormatID &&
           f->mFormatFlags == ours.mFormatFlags && f->mBytesPerPacket == ours.mBytesPerPacket &&
           f->mFramesPerPacket == ours.mFramesPerPacket &&
           f->mBytesPerFrame == ours.mBytesPerFrame &&
           f->mChannelsPerFrame == ours.mChannelsPerFrame &&
           f->mBitsPerChannel == ours.mBitsPerChannel;
}

/* ---- IUnknown --------------------------------------------------------- */

static HRESULT cj_QueryInterface(void *driver, REFIID uuid, LPVOID *out);
static ULONG cj_AddRef(void *driver);
static ULONG cj_Release(void *driver);
static OSStatus cj_Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef h);
static OSStatus cj_CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef description,
                                const AudioServerPlugInClientInfo *client, AudioObjectID *out);
static OSStatus cj_DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID device);
static OSStatus cj_AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                   const AudioServerPlugInClientInfo *client);
static OSStatus cj_RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                      const AudioServerPlugInClientInfo *client);
static OSStatus cj_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                                    AudioObjectID device, UInt64 action,
                                                    void *info);
static OSStatus cj_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                                  AudioObjectID device, UInt64 action,
                                                  void *info);
static Boolean cj_HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t pid,
                              const AudioObjectPropertyAddress *address);
static OSStatus cj_IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                      pid_t pid, const AudioObjectPropertyAddress *address,
                                      Boolean *out);
static OSStatus cj_GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                       pid_t pid, const AudioObjectPropertyAddress *address,
                                       UInt32 qualifier_size, const void *qualifier,
                                       UInt32 *out_size);
static OSStatus cj_GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                   pid_t pid, const AudioObjectPropertyAddress *address,
                                   UInt32 qualifier_size, const void *qualifier, UInt32 in_size,
                                   UInt32 *out_size, void *out);
static OSStatus cj_SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                   pid_t pid, const AudioObjectPropertyAddress *address,
                                   UInt32 qualifier_size, const void *qualifier, UInt32 in_size,
                                   const void *in);
static OSStatus cj_StartIO(AudioServerPlugInDriverRef driver, AudioObjectID device,
                           UInt32 client);
static OSStatus cj_StopIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client);
static OSStatus cj_GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                    UInt32 client, Float64 *sample_time, UInt64 *host_time,
                                    UInt64 *seed);
static OSStatus cj_WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                     UInt32 client, UInt32 operation, Boolean *will_do,
                                     Boolean *in_place);
static OSStatus cj_BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                    UInt32 client, UInt32 operation, UInt32 frames,
                                    const AudioServerPlugInIOCycleInfo *cycle);
static OSStatus cj_DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                 AudioObjectID stream, UInt32 client, UInt32 operation,
                                 UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle,
                                 void *main_buffer, void *secondary_buffer);
static OSStatus cj_EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                  UInt32 client, UInt32 operation, UInt32 frames,
                                  const AudioServerPlugInIOCycleInfo *cycle);

static AudioServerPlugInDriverInterface driver_interface = {
    NULL,
    cj_QueryInterface,
    cj_AddRef,
    cj_Release,
    cj_Initialize,
    cj_CreateDevice,
    cj_DestroyDevice,
    cj_AddDeviceClient,
    cj_RemoveDeviceClient,
    cj_PerformDeviceConfigurationChange,
    cj_AbortDeviceConfigurationChange,
    cj_HasProperty,
    cj_IsPropertySettable,
    cj_GetPropertyDataSize,
    cj_GetPropertyData,
    cj_SetPropertyData,
    cj_StartIO,
    cj_StopIO,
    cj_GetZeroTimeStamp,
    cj_WillDoIOOperation,
    cj_BeginIOOperation,
    cj_DoIOOperation,
    cj_EndIOOperation,
};
static AudioServerPlugInDriverInterface *driver_interface_ptr = &driver_interface;
static AudioServerPlugInDriverRef const driver_ref = &driver_interface_ptr;

static bool is_our_driver(const void *driver) { return driver == (const void *)driver_ref; }

/* The CFPlugIn factory named in Info.plist: the bundle's only export. */
__attribute__((visibility("default"))) void *CursorJumpAudio_Create(CFAllocatorRef allocator,
                                                                    CFUUIDRef requested_type);
void *CursorJumpAudio_Create(CFAllocatorRef allocator, CFUUIDRef requested_type) {
    (void)allocator;
    if (!CFEqual(requested_type, kAudioServerPlugInTypeUUID)) return NULL;
    atomic_fetch_add(&ref_count, 1);
    return (void *)driver_ref;
}

static HRESULT cj_QueryInterface(void *driver, REFIID uuid, LPVOID *out) {
    if (!is_our_driver(driver) || out == NULL) return kAudioHardwareBadObjectError;
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(NULL, uuid);
    if (requested == NULL) return kAudioHardwareUnspecifiedError;
    bool known = CFEqual(requested, IUnknownUUID) ||
                 CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID);
    CFRelease(requested);
    if (!known) {
        *out = NULL;
        return E_NOINTERFACE;
    }
    atomic_fetch_add(&ref_count, 1);
    *out = (LPVOID)driver_ref;
    return S_OK;
}

static ULONG cj_AddRef(void *driver) {
    if (!is_our_driver(driver)) return 0;
    return atomic_fetch_add(&ref_count, 1) + 1;
}

static ULONG cj_Release(void *driver) {
    if (!is_our_driver(driver)) return 0;
    ULONG current = atomic_load(&ref_count);
    while (current > 0 && !atomic_compare_exchange_weak(&ref_count, &current, current - 1)) {
    }
    /* The driver is static: there is nothing to free at 0. */
    return current > 0 ? current - 1 : 0;
}

/* ---- Basic operations ------------------------------------------------- */

static OSStatus cj_Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef h) {
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    host = h;
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    /* Host ticks per second = 1e9 * denom / numer. */
    host_ticks_per_frame = (1e9 * (Float64)tb.denom / (Float64)tb.numer) / CJ_SAMPLE_RATE;
    notify_queue = dispatch_queue_create("com.cursorjump.audio.notify", DISPATCH_QUEUE_SERIAL);
    cj_ring_reset(&mic_ring);
    return kAudioHardwareNoError;
}

static OSStatus cj_CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef description,
                                const AudioServerPlugInClientInfo *client, AudioObjectID *out) {
    (void)driver, (void)description, (void)client, (void)out;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus cj_DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID device) {
    (void)driver, (void)device;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus cj_AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                   const AudioServerPlugInClientInfo *client) {
    (void)client;
    if (!is_our_driver(driver) || device_by_id(device) == NULL)
        return kAudioHardwareBadObjectError;
    return kAudioHardwareNoError;
}

static OSStatus cj_RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                      const AudioServerPlugInClientInfo *client) {
    (void)client;
    if (!is_our_driver(driver) || device_by_id(device) == NULL)
        return kAudioHardwareBadObjectError;
    return kAudioHardwareNoError;
}

/* No configuration changes are ever requested: the format is fixed. */
static OSStatus cj_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                                    AudioObjectID device, UInt64 action,
                                                    void *info) {
    (void)action, (void)info;
    if (!is_our_driver(driver) || device_by_id(device) == NULL)
        return kAudioHardwareBadObjectError;
    return kAudioHardwareNoError;
}

static OSStatus cj_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                                  AudioObjectID device, UInt64 action,
                                                  void *info) {
    (void)action, (void)info;
    if (!is_our_driver(driver) || device_by_id(device) == NULL)
        return kAudioHardwareBadObjectError;
    return kAudioHardwareNoError;
}

/* ---- Property helpers ------------------------------------------------- */

/*
 * Every property getter below follows one pattern: it sets *size to the
 * property's full size and, when out is not NULL, writes the value. So the
 * same code answers HasProperty, GetPropertyDataSize and GetPropertyData.
 */
typedef struct {
    UInt32 in_size; /* bytes available at out */
    void *out;      /* NULL: size only */
    UInt32 *size;   /* bytes the property has (on a size query) or wrote */
} cj_reply;

static OSStatus put_bytes(cj_reply *r, const void *value, UInt32 n) {
    *r->size = n;
    if (r->out == NULL) return kAudioHardwareNoError;
    if (r->in_size < n) return kAudioHardwareBadPropertySizeError;
    memcpy(r->out, value, n);
    return kAudioHardwareNoError;
}

static OSStatus put_u32(cj_reply *r, UInt32 v) { return put_bytes(r, &v, sizeof v); }
static OSStatus put_f64(cj_reply *r, Float64 v) { return put_bytes(r, &v, sizeof v); }
static OSStatus put_id(cj_reply *r, AudioObjectID v) { return put_bytes(r, &v, sizeof v); }

/* The caller releases the string; CFSTR constants are never freed. */
static OSStatus put_string(cj_reply *r, CFStringRef s) { return put_bytes(r, &s, sizeof s); }

/* Arrays are written as far as they fit, the way the HAL expects. */
static OSStatus put_array(cj_reply *r, const void *items, UInt32 count, UInt32 item_size) {
    if (r->out == NULL) {
        *r->size = count * item_size;
        return kAudioHardwareNoError;
    }
    UInt32 fit = r->in_size / item_size;
    if (fit > count) fit = count;
    memcpy(r->out, items, (size_t)fit * item_size);
    *r->size = fit * item_size;
    return kAudioHardwareNoError;
}

static OSStatus put_ids(cj_reply *r, const AudioObjectID *ids, UInt32 count) {
    return put_array(r, ids, count, sizeof(AudioObjectID));
}

/* OwnedObjects may be filtered by a qualifier: an array of AudioClassIDs. */
static bool class_wanted(AudioClassID cls, AudioClassID base, UInt32 qualifier_size,
                         const void *qualifier) {
    UInt32 n = qualifier_size / sizeof(AudioClassID);
    if (qualifier == NULL || n == 0) return true;
    const AudioClassID *wanted = qualifier;
    for (UInt32 i = 0; i < n; i++)
        if (wanted[i] == cls || wanted[i] == base || wanted[i] == kAudioObjectClassIDWildcard)
            return true;
    return false;
}

/* Whether a device's one stream is in the given scope. */
static bool stream_in_scope(const cj_device *d, AudioObjectPropertyScope scope) {
    switch (scope) {
    case kAudioObjectPropertyScopeGlobal:
    case kAudioObjectPropertyScopeWildcard:
        return true;
    case kAudioObjectPropertyScopeInput:
        return d->is_input;
    case kAudioObjectPropertyScopeOutput:
        return !d->is_input;
    default:
        return false;
    }
}

static UInt32 io_count_of(cj_device *d) {
    pthread_mutex_lock(&state_lock);
    UInt32 n = d->io_count;
    pthread_mutex_unlock(&state_lock);
    return n;
}

/* ---- Property getters, by object ------------------------------------- */

static OSStatus plugin_property(const AudioObjectPropertyAddress *a, UInt32 qualifier_size,
                                const void *qualifier, cj_reply *r) {
    AudioObjectID all_devices[CJ_DEVICE_COUNT];
    for (UInt32 i = 0; i < CJ_DEVICE_COUNT; i++) all_devices[i] = devices[i].id;

    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass:
        return put_u32(r, kAudioObjectClassID);
    case kAudioObjectPropertyClass:
        return put_u32(r, kAudioPlugInClassID);
    case kAudioObjectPropertyOwner:
        return put_id(r, kAudioObjectUnknown);
    case kAudioObjectPropertyManufacturer:
        return put_string(r, CJ_MANUFACTURER);
    case kAudioObjectPropertyOwnedObjects:
        if (!class_wanted(kAudioDeviceClassID, kAudioObjectClassID, qualifier_size, qualifier))
            return put_ids(r, NULL, 0);
        return put_ids(r, all_devices, CJ_DEVICE_COUNT);
    case kAudioPlugInPropertyDeviceList:
        return put_ids(r, all_devices, CJ_DEVICE_COUNT);
    case kAudioPlugInPropertyBoxList:
        return put_ids(r, NULL, 0);
    case kAudioPlugInPropertyBundleID:
        return put_string(r, CJ_PLUGIN_BUNDLE_ID);
    case kAudioPlugInPropertyResourceBundle:
        return put_string(r, CFSTR(""));
    case kAudioPlugInPropertyTranslateUIDToDevice: {
        AudioObjectID found = kAudioObjectUnknown;
        if (r->out != NULL) {
            if (qualifier_size != sizeof(CFStringRef) || qualifier == NULL)
                return kAudioHardwareBadPropertySizeError;
            CFStringRef uid = *(const CFStringRef *)qualifier;
            for (UInt32 i = 0; uid != NULL && i < CJ_DEVICE_COUNT; i++)
                if (CFEqual(uid, devices[i].uid)) found = devices[i].id;
        }
        return put_id(r, found);
    }
    default:
        return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus device_property(cj_device *d, const AudioObjectPropertyAddress *a,
                                UInt32 qualifier_size, const void *qualifier, cj_reply *r) {
    bool has_stream = stream_in_scope(d, a->mScope);
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass:
        return put_u32(r, kAudioObjectClassID);
    case kAudioObjectPropertyClass:
        return put_u32(r, kAudioDeviceClassID);
    case kAudioObjectPropertyOwner:
        return put_id(r, kObjectID_PlugIn);
    case kAudioObjectPropertyName:
        return put_string(r, d->name);
    case kAudioObjectPropertyManufacturer:
        return put_string(r, CJ_MANUFACTURER);
    case kAudioObjectPropertyOwnedObjects:
        if (!has_stream ||
            !class_wanted(kAudioStreamClassID, kAudioObjectClassID, qualifier_size, qualifier))
            return put_ids(r, NULL, 0);
        return put_ids(r, &d->stream_id, 1);
    case kAudioDevicePropertyStreams:
        return put_ids(r, &d->stream_id, has_stream ? 1 : 0);
    case kAudioObjectPropertyControlList:
        return put_ids(r, NULL, 0);
    case kAudioDevicePropertyDeviceUID:
        return put_string(r, d->uid);
    case kAudioDevicePropertyModelUID:
        return put_string(r, CJ_MODEL_UID);
    case kAudioDevicePropertyTransportType:
        return put_u32(r, kAudioDeviceTransportTypeVirtual);
    case kAudioDevicePropertyRelatedDevices:
        return put_ids(r, &d->id, 1);
    case kAudioDevicePropertyClockDomain:
        return put_u32(r, CJ_CLOCK_DOMAIN);
    case kAudioDevicePropertyDeviceIsAlive:
        return put_u32(r, 1);
    case kAudioDevicePropertyDeviceIsRunning:
    case kAudioDevicePropertyDeviceIsRunningSomewhere:
        return put_u32(r, r->out != NULL && io_count_of(d) > 0 ? 1 : 0);
    case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        return put_u32(r, d->can_be_default ? 1 : 0);
    case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        return put_u32(r, 0);
    case kAudioDevicePropertyIsHidden:
        return put_u32(r, d->is_hidden ? 1 : 0);
    case kAudioDevicePropertyLatency:
    case kAudioDevicePropertySafetyOffset:
        return put_u32(r, 0);
    case kAudioDevicePropertyNominalSampleRate:
        return put_f64(r, CJ_SAMPLE_RATE);
    case kAudioDevicePropertyAvailableNominalSampleRates: {
        AudioValueRange rate = {CJ_SAMPLE_RATE, CJ_SAMPLE_RATE};
        return put_array(r, &rate, 1, sizeof rate);
    }
    case kAudioDevicePropertyBufferFrameSizeRange: {
        AudioValueRange range = {CJ_MIN_BUFFER_FRAMES, CJ_MAX_BUFFER_FRAMES};
        return put_bytes(r, &range, sizeof range);
    }
    case kAudioDevicePropertyZeroTimeStampPeriod:
        return put_u32(r, CJ_ZERO_TIMESTAMP_PERIOD);
    case kAudioDevicePropertyPreferredChannelsForStereo: {
        UInt32 channels[2] = {1, 1};
        return put_bytes(r, channels, sizeof channels);
    }
    case kAudioDevicePropertyPreferredChannelLayout: {
        /* AudioChannelLayout already holds one channel description. */
        AudioChannelLayout layout;
        memset(&layout, 0, sizeof layout);
        layout.mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
        layout.mNumberChannelDescriptions = 1;
        layout.mChannelDescriptions[0].mChannelLabel = kAudioChannelLabel_Mono;
        return put_bytes(r, &layout, sizeof layout);
    }
    default:
        return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus stream_property(cj_device *d, const AudioObjectPropertyAddress *a, cj_reply *r) {
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass:
        return put_u32(r, kAudioObjectClassID);
    case kAudioObjectPropertyClass:
        return put_u32(r, kAudioStreamClassID);
    case kAudioObjectPropertyOwner:
        return put_id(r, d->id);
    case kAudioObjectPropertyOwnedObjects:
        return put_ids(r, NULL, 0);
    case kAudioStreamPropertyIsActive: {
        UInt32 active = 1;
        if (r->out != NULL) {
            pthread_mutex_lock(&state_lock);
            active = d->stream_is_active;
            pthread_mutex_unlock(&state_lock);
        }
        return put_u32(r, active);
    }
    case kAudioStreamPropertyDirection:
        return put_u32(r, d->is_input ? 1 : 0);
    case kAudioStreamPropertyTerminalType:
        return put_u32(r, d->is_input ? kAudioStreamTerminalTypeMicrophone
                                      : kAudioStreamTerminalTypeLine);
    case kAudioStreamPropertyStartingChannel:
        return put_u32(r, 1);
    case kAudioStreamPropertyLatency:
        return put_u32(r, 0);
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat: {
        AudioStreamBasicDescription f = stream_format();
        return put_bytes(r, &f, sizeof f);
    }
    case kAudioStreamPropertyAvailableVirtualFormats:
    case kAudioStreamPropertyAvailablePhysicalFormats: {
        AudioStreamRangedDescription f = {
            .mFormat = stream_format(),
            .mSampleRateRange = {CJ_SAMPLE_RATE, CJ_SAMPLE_RATE},
        };
        return put_array(r, &f, 1, sizeof f);
    }
    default:
        return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus object_property(AudioObjectID object, const AudioObjectPropertyAddress *a,
                                UInt32 qualifier_size, const void *qualifier, cj_reply *r) {
    if (a == NULL) return kAudioHardwareIllegalOperationError;
    if (object == kObjectID_PlugIn) return plugin_property(a, qualifier_size, qualifier, r);
    cj_device *d = device_by_id(object);
    if (d != NULL) return device_property(d, a, qualifier_size, qualifier, r);
    d = device_by_stream(object);
    if (d != NULL) return stream_property(d, a, r);
    return kAudioHardwareBadObjectError;
}

/* ---- Property entry points ------------------------------------------- */

static Boolean cj_HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t pid,
                              const AudioObjectPropertyAddress *address) {
    (void)pid;
    if (!is_our_driver(driver)) return false;
    UInt32 size = 0;
    cj_reply r = {0, NULL, &size};
    return object_property(object, address, 0, NULL, &r) == kAudioHardwareNoError;
}

static OSStatus cj_IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                      pid_t pid, const AudioObjectPropertyAddress *address,
                                      Boolean *out) {
    (void)pid;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    if (out == NULL) return kAudioHardwareIllegalOperationError;
    UInt32 size = 0;
    cj_reply r = {0, NULL, &size};
    OSStatus err = object_property(object, address, 0, NULL, &r);
    if (err != kAudioHardwareNoError) return err;
    /* Only a stream's active flag can change; the format and rate are fixed. */
    *out = device_by_stream(object) != NULL && address->mSelector == kAudioStreamPropertyIsActive;
    return kAudioHardwareNoError;
}

static OSStatus cj_GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                       pid_t pid, const AudioObjectPropertyAddress *address,
                                       UInt32 qualifier_size, const void *qualifier,
                                       UInt32 *out_size) {
    (void)pid;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    if (out_size == NULL) return kAudioHardwareIllegalOperationError;
    cj_reply r = {0, NULL, out_size};
    return object_property(object, address, qualifier_size, qualifier, &r);
}

static OSStatus cj_GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                   pid_t pid, const AudioObjectPropertyAddress *address,
                                   UInt32 qualifier_size, const void *qualifier, UInt32 in_size,
                                   UInt32 *out_size, void *out) {
    (void)pid;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    if (out_size == NULL || out == NULL) return kAudioHardwareIllegalOperationError;
    cj_reply r = {in_size, out, out_size};
    return object_property(object, address, qualifier_size, qualifier, &r);
}

static OSStatus cj_SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                   pid_t pid, const AudioObjectPropertyAddress *address,
                                   UInt32 qualifier_size, const void *qualifier, UInt32 in_size,
                                   const void *in) {
    (void)pid, (void)qualifier_size, (void)qualifier;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    if (address == NULL || in == NULL) return kAudioHardwareIllegalOperationError;
    cj_device *stream_owner = device_by_stream(object);

    if (stream_owner != NULL && address->mSelector == kAudioStreamPropertyIsActive) {
        if (in_size != sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
        pthread_mutex_lock(&state_lock);
        stream_owner->stream_is_active = *(const UInt32 *)in != 0;
        pthread_mutex_unlock(&state_lock);
        return kAudioHardwareNoError;
    }
    /* Setting the one supported rate or format is accepted as a no-op. */
    if (device_by_id(object) != NULL &&
        address->mSelector == kAudioDevicePropertyNominalSampleRate) {
        if (in_size != sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
        return *(const Float64 *)in == CJ_SAMPLE_RATE ? kAudioHardwareNoError
                                                      : kAudioDeviceUnsupportedFormatError;
    }
    if (stream_owner != NULL && (address->mSelector == kAudioStreamPropertyVirtualFormat ||
                                 address->mSelector == kAudioStreamPropertyPhysicalFormat)) {
        if (in_size != sizeof(AudioStreamBasicDescription))
            return kAudioHardwareBadPropertySizeError;
        return is_our_format(in) ? kAudioHardwareNoError : kAudioDeviceUnsupportedFormatError;
    }

    UInt32 size = 0;
    cj_reply r = {0, NULL, &size};
    OSStatus err = object_property(object, address, 0, NULL, &r);
    return err != kAudioHardwareNoError ? err : kAudioHardwareUnsupportedOperationError;
}

/* ---- IO control and the shared clock --------------------------------- */

/* Tells the host that a device's running state changed. Runs on
 * notify_queue, never inside a host call. */
static void notify_running_changed(void *context) {
    AudioObjectID device = (AudioObjectID)(uintptr_t)context;
    AudioObjectPropertyAddress changed[2] = {
        {kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal,
         kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioObjectPropertyScopeGlobal,
         kAudioObjectPropertyElementMain},
    };
    if (host != NULL) host->PropertiesChanged(host, device, 2, changed);
}

static bool any_device_running_locked(void) {
    for (UInt32 i = 0; i < CJ_DEVICE_COUNT; i++)
        if (devices[i].io_count > 0) return true;
    return false;
}

static OSStatus cj_StartIO(AudioServerPlugInDriverRef driver, AudioObjectID device,
                           UInt32 client) {
    (void)client;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    cj_device *d = device_by_id(device);
    if (d == NULL) return kAudioHardwareBadObjectError;

    pthread_mutex_lock(&state_lock);
    if (d->io_count == UINT32_MAX) {
        pthread_mutex_unlock(&state_lock);
        return kAudioHardwareIllegalOperationError;
    }
    if (!any_device_running_locked()) {
        /* The first device to start anchors the shared clock. */
        cj_ring_reset(&mic_ring);
        atomic_store_explicit(&anchor_host_time, mach_absolute_time(), memory_order_release);
    }
    bool started = d->io_count++ == 0;
    pthread_mutex_unlock(&state_lock);

    if (started) dispatch_async_f(notify_queue, (void *)(uintptr_t)d->id, notify_running_changed);
    return kAudioHardwareNoError;
}

static OSStatus cj_StopIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client) {
    (void)client;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    cj_device *d = device_by_id(device);
    if (d == NULL) return kAudioHardwareBadObjectError;

    pthread_mutex_lock(&state_lock);
    if (d->io_count == 0) {
        pthread_mutex_unlock(&state_lock);
        return kAudioHardwareIllegalOperationError;
    }
    bool stopped = --d->io_count == 0;
    /* The last device to stop clears the shared clock. */
    if (!any_device_running_locked())
        atomic_store_explicit(&anchor_host_time, 0, memory_order_release);
    pthread_mutex_unlock(&state_lock);

    if (stopped) dispatch_async_f(notify_queue, (void *)(uintptr_t)d->id, notify_running_changed);
    return kAudioHardwareNoError;
}

/*
 * Both devices return the same sequence: sample time k * period at host time
 * anchor + k * period * ticks-per-frame, for the latest k that has passed.
 */
static OSStatus cj_GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                    UInt32 client, Float64 *sample_time, UInt64 *host_time,
                                    UInt64 *seed) {
    (void)driver, (void)device, (void)client;
    UInt64 anchor = atomic_load_explicit(&anchor_host_time, memory_order_acquire);
    if (anchor == 0) return kAudioHardwareNotRunningError;
    Float64 ticks_per_period = host_ticks_per_frame * CJ_ZERO_TIMESTAMP_PERIOD;
    UInt64 now = mach_absolute_time();
    UInt64 periods = now > anchor ? (UInt64)((Float64)(now - anchor) / ticks_per_period) : 0;
    *sample_time = (Float64)periods * CJ_ZERO_TIMESTAMP_PERIOD;
    *host_time = anchor + (UInt64)((Float64)periods * ticks_per_period);
    *seed = 1;
    return kAudioHardwareNoError;
}

/* ---- IO --------------------------------------------------------------- */

static OSStatus cj_WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                     UInt32 client, UInt32 operation, Boolean *will_do,
                                     Boolean *in_place) {
    (void)client;
    if (!is_our_driver(driver)) return kAudioHardwareBadObjectError;
    cj_device *d = device_by_id(device);
    if (d == NULL) return kAudioHardwareBadObjectError;
    bool does = d->is_input ? operation == kAudioServerPlugInIOOperationReadInput
                            : operation == kAudioServerPlugInIOOperationWriteMix;
    if (will_do != NULL) *will_do = does;
    if (in_place != NULL) *in_place = true;
    return kAudioHardwareNoError;
}

static OSStatus cj_BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                    UInt32 client, UInt32 operation, UInt32 frames,
                                    const AudioServerPlugInIOCycleInfo *cycle) {
    (void)driver, (void)device, (void)client, (void)operation, (void)frames, (void)cycle;
    return kAudioHardwareNoError;
}

/*
 * Early in a run the input time can be before the clock's sample time 0.
 * Returns how many leading frames are before 0 (they have no ring slot) and
 * sets *start to the sample time of the first frame after them.
 */
static UInt32 frames_before_zero(Float64 sample_time, UInt32 frames, uint64_t *start) {
    if (sample_time >= 0) {
        *start = (uint64_t)sample_time;
        return 0;
    }
    *start = 0;
    Float64 before = -sample_time;
    return before >= (Float64)frames ? frames : (UInt32)before;
}

static OSStatus cj_DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                 AudioObjectID stream, UInt32 client, UInt32 operation,
                                 UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle,
                                 void *main_buffer, void *secondary_buffer) {
    (void)driver, (void)stream, (void)client, (void)secondary_buffer;
    if (main_buffer == NULL || cycle == NULL) return kAudioHardwareNoError;
    float *buf = main_buffer;

    if (device == kObjectID_Mic && operation == kAudioServerPlugInIOOperationReadInput) {
        uint64_t t;
        UInt32 skip = frames_before_zero(cycle->mInputTime.mSampleTime, frames, &t);
        memset(buf, 0, (size_t)skip * sizeof(float));
        if (skip < frames) cj_ring_read(&mic_ring, t, buf + skip, frames - skip);
    } else if (device == kObjectID_Feed && operation == kAudioServerPlugInIOOperationWriteMix) {
        uint64_t t;
        UInt32 skip = frames_before_zero(cycle->mOutputTime.mSampleTime, frames, &t);
        if (skip < frames) cj_ring_write(&mic_ring, t, buf + skip, frames - skip);
    }
    return kAudioHardwareNoError;
}

static OSStatus cj_EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                  UInt32 client, UInt32 operation, UInt32 frames,
                                  const AudioServerPlugInIOCycleInfo *cycle) {
    (void)driver, (void)device, (void)client, (void)operation, (void)frames, (void)cycle;
    return kAudioHardwareNoError;
}
