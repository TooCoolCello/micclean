// The MicClean audio device for macOS: an Audio Server plug-in, which runs inside Core Audio's
// own process (no kernel extension, no Apple licence). One device, "MicClean", whose output
// loops back to its input: the MicClean app plays the cleaned voice into it, and Discord (or
// any app) records it as a mic.
//
// Written from Apple's AudioServerPlugIn.h; no code from other virtual audio drivers.
//
// Objects: the plug-in (1), the device (2), its input stream (3) and output stream (4).
// Format: 2 channels of 32-bit float, 44.1 or 48 kHz (48 by default).
// Clock: the host clock, one zero time stamp every RING frames.
// Loopback: the output writes into a ring at its sample time, the input reads the same sample
// times back. Frames nobody wrote (MicClean not running) read as silence, not old sound.

#include <CoreAudio/AudioServerPlugIn.h>
#include <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stddef.h>
#include <string.h>

enum { kPlugIn = kAudioObjectPlugInObject, kDevice = 2, kInStream = 3, kOutStream = 4 };

#define CHANNELS 2
#define RING 16384 // frames: about 1/3 s, far more than any IO cycle
#define DEVICE_UID "MicClean_Device"
#define MODEL_UID "MicClean_Model"

static const Float64 kRates[] = {44100.0, 48000.0};
#define NRATES (sizeof kRates / sizeof kRates[0])

// gLock guards everything but the ring and gWriteEnd, which only the IO thread touches.
static pthread_mutex_t gLock = PTHREAD_MUTEX_INITIALIZER;
static AudioServerPlugInHostRef gHost;
static UInt32 gRefs = 1;
static Float64 gRate = 48000.0;
static Float64 gTicksPerFrame;
static UInt32 gClients;      // clients doing IO
static UInt64 gAnchor;       // host time of sample time 0
static UInt64 gStamps;       // zero time stamps since gAnchor
static UInt32 gInActive = 1, gOutActive = 1;
static float gRing[RING * CHANNELS];
static Float64 gWriteEnd;    // sample time just after the newest frame written

static void SetTicks(void) {
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    gTicksPerFrame = (1e9 * tb.denom / tb.numer) / gRate;
}

static Boolean IsRate(Float64 r) {
    for (UInt32 i = 0; i < NRATES; i++)
        if (r == kRates[i]) return true;
    return false;
}

static AudioStreamBasicDescription Format(Float64 rate) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = rate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBytesPerPacket = 4 * CHANNELS;
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = 4 * CHANNELS;
    f.mChannelsPerFrame = CHANNELS;
    f.mBitsPerChannel = 32;
    return f;
}

// A rate change has to go through the host, which calls PerformDeviceConfigurationChange back
// once no IO is running. The rate travels as the change action.
static void AskForRate(Float64 rate) {
    dispatch_async(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0), ^{
        gHost->RequestDeviceConfigurationChange(gHost, kDevice, (UInt64)rate, NULL);
    });
}

// ---------------------------------------------------------------------------------------------
// Properties. One function answers both "how big" (out == NULL) and "what is it".

static OSStatus Put(const void* src, UInt32 size, UInt32 inSize, UInt32* outSize, void* out) {
    if (out) {
        if (inSize < size) return kAudioHardwareBadPropertySizeError;
        memcpy(out, src, size);
    }
    *outSize = size;
    return kAudioHardwareNoError;
}

// Lists: as many items as fit.
static OSStatus PutList(const void* src, UInt32 count, UInt32 item, UInt32 inSize, UInt32* outSize, void* out) {
    if (out && inSize / item < count) count = inSize / item;
    if (out && count) memcpy(out, src, count * item);
    *outSize = count * item;
    return kAudioHardwareNoError;
}

#define PUT(value) do { __typeof__(value) v_ = (value); return Put(&v_, sizeof v_, inSize, outSize, out); } while (0)
#define PUT_STR(s) do { CFStringRef v_ = CFSTR(s); return Put(&v_, sizeof v_, inSize, outSize, out); } while (0)

static OSStatus Prop(AudioObjectID obj, const AudioObjectPropertyAddress* a, UInt32 qSize, const void* q, UInt32 inSize, UInt32* outSize, void* out) {
    AudioObjectPropertySelector sel = a->mSelector;
    AudioObjectPropertyScope scope = a->mScope;
    pthread_mutex_lock(&gLock);
    Float64 rate = gRate;
    UInt32 clients = gClients, inActive = gInActive, outActive = gOutActive;
    pthread_mutex_unlock(&gLock);

    switch (obj) {
    case kPlugIn:
        switch (sel) {
        case kAudioObjectPropertyBaseClass: PUT((AudioClassID)kAudioObjectClassID);
        case kAudioObjectPropertyClass: PUT((AudioClassID)kAudioPlugInClassID);
        case kAudioObjectPropertyOwner: PUT((AudioObjectID)kAudioObjectUnknown);
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer: PUT_STR("MicClean");
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: {
            AudioObjectID list[] = {kDevice};
            return PutList(list, 1, sizeof list[0], inSize, outSize, out);
        }
        case kAudioPlugInPropertyBoxList: return PutList(NULL, 0, sizeof(AudioObjectID), inSize, outSize, out);
        case kAudioPlugInPropertyTranslateUIDToDevice: {
            AudioObjectID found = kAudioObjectUnknown;
            if (out && qSize == sizeof(CFStringRef) && q && CFEqual(*(const CFStringRef*)q, CFSTR(DEVICE_UID))) found = kDevice;
            PUT(found);
        }
        case kAudioPlugInPropertyResourceBundle: PUT_STR("");
        }
        break;

    case kDevice:
        switch (sel) {
        case kAudioObjectPropertyBaseClass: PUT((AudioClassID)kAudioObjectClassID);
        case kAudioObjectPropertyClass: PUT((AudioClassID)kAudioDeviceClassID);
        case kAudioObjectPropertyOwner: PUT((AudioObjectID)kPlugIn);
        case kAudioObjectPropertyName: PUT_STR("MicClean");
        case kAudioObjectPropertyManufacturer: PUT_STR("MicClean");
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyStreams: {
            AudioObjectID both[] = {kInStream, kOutStream};
            if (scope == kAudioObjectPropertyScopeInput) return PutList(&both[0], 1, sizeof both[0], inSize, outSize, out);
            if (scope == kAudioObjectPropertyScopeOutput) return PutList(&both[1], 1, sizeof both[0], inSize, outSize, out);
            return PutList(both, 2, sizeof both[0], inSize, outSize, out);
        }
        case kAudioObjectPropertyControlList: return PutList(NULL, 0, sizeof(AudioObjectID), inSize, outSize, out);
        case kAudioDevicePropertyDeviceUID: PUT_STR(DEVICE_UID);
        case kAudioDevicePropertyModelUID: PUT_STR(MODEL_UID);
        case kAudioDevicePropertyTransportType: PUT((UInt32)kAudioDeviceTransportTypeVirtual);
        case kAudioDevicePropertyRelatedDevices: {
            AudioObjectID list[] = {kDevice};
            return PutList(list, 1, sizeof list[0], inSize, outSize, out);
        }
        case kAudioDevicePropertyClockDomain: PUT((UInt32)0);
        case kAudioDevicePropertyDeviceIsAlive: PUT((UInt32)1);
        case kAudioDevicePropertyDeviceIsRunning: PUT((UInt32)(clients > 0));
        // A mic people pick on purpose; never where the Mac's own sound goes.
        case kAudioDevicePropertyDeviceCanBeDefaultDevice: PUT((UInt32)(scope == kAudioObjectPropertyScopeInput));
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: PUT((UInt32)0);
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset: PUT((UInt32)0);
        case kAudioDevicePropertyNominalSampleRate: PUT(rate);
        case kAudioDevicePropertyAvailableNominalSampleRates: {
            AudioValueRange list[NRATES];
            for (UInt32 i = 0; i < NRATES; i++) list[i] = (AudioValueRange){kRates[i], kRates[i]};
            return PutList(list, NRATES, sizeof list[0], inSize, outSize, out);
        }
        case kAudioDevicePropertyIsHidden: PUT((UInt32)0);
        case kAudioDevicePropertyPreferredChannelsForStereo: {
            UInt32 pair[2] = {1, 2};
            return Put(pair, sizeof pair, inSize, outSize, out);
        }
        case kAudioDevicePropertyPreferredChannelLayout: {
            UInt32 size = (UInt32)(offsetof(AudioChannelLayout, mChannelDescriptions) + CHANNELS * sizeof(AudioChannelDescription));
            if (out) {
                if (inSize < size) return kAudioHardwareBadPropertySizeError;
                AudioChannelLayout* l = out;
                memset(l, 0, size);
                l->mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
                l->mNumberChannelDescriptions = CHANNELS;
                l->mChannelDescriptions[0].mChannelLabel = kAudioChannelLabel_Left;
                l->mChannelDescriptions[1].mChannelLabel = kAudioChannelLabel_Right;
            }
            *outSize = size;
            return kAudioHardwareNoError;
        }
        case kAudioDevicePropertyZeroTimeStampPeriod: PUT((UInt32)RING);
        }
        break;

    case kInStream:
    case kOutStream: {
        Boolean input = obj == kInStream;
        switch (sel) {
        case kAudioObjectPropertyBaseClass: PUT((AudioClassID)kAudioObjectClassID);
        case kAudioObjectPropertyClass: PUT((AudioClassID)kAudioStreamClassID);
        case kAudioObjectPropertyOwner: PUT((AudioObjectID)kDevice);
        case kAudioObjectPropertyOwnedObjects: return PutList(NULL, 0, sizeof(AudioObjectID), inSize, outSize, out);
        case kAudioStreamPropertyIsActive: PUT(input ? inActive : outActive);
        case kAudioStreamPropertyDirection: PUT((UInt32)input);
        case kAudioStreamPropertyTerminalType: PUT((UInt32)(input ? kAudioStreamTerminalTypeMicrophone : kAudioStreamTerminalTypeSpeaker));
        case kAudioStreamPropertyStartingChannel: PUT((UInt32)1);
        case kAudioStreamPropertyLatency: PUT((UInt32)0);
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat: PUT(Format(rate));
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: {
            AudioStreamRangedDescription list[NRATES];
            for (UInt32 i = 0; i < NRATES; i++) list[i] = (AudioStreamRangedDescription){Format(kRates[i]), {kRates[i], kRates[i]}};
            return PutList(list, NRATES, sizeof list[0], inSize, outSize, out);
        }
        }
        break;
    }

    default:
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareUnknownPropertyError;
}

static Boolean HasProperty(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress* a) {
    (void)d, (void)pid;
    UInt32 size;
    return Prop(obj, a, 0, NULL, 0, &size, NULL) == kAudioHardwareNoError;
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress* a, Boolean* out) {
    if (!HasProperty(d, obj, pid, a)) return obj <= kOutStream ? kAudioHardwareUnknownPropertyError : kAudioHardwareBadObjectError;
    switch (a->mSelector) {
    case kAudioDevicePropertyNominalSampleRate: *out = obj == kDevice; break;
    case kAudioStreamPropertyIsActive:
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat: *out = obj == kInStream || obj == kOutStream; break;
    default: *out = false;
    }
    return kAudioHardwareNoError;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress* a, UInt32 qSize, const void* q, UInt32* outSize) {
    (void)d, (void)pid;
    return Prop(obj, a, qSize, q, 0, outSize, NULL);
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress* a, UInt32 qSize, const void* q, UInt32 inSize, UInt32* outSize, void* out) {
    (void)d, (void)pid;
    if (!out) return kAudioHardwareIllegalOperationError;
    return Prop(obj, a, qSize, q, inSize, outSize, out);
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress* a, UInt32 qSize, const void* q, UInt32 inSize, const void* in) {
    (void)d, (void)pid, (void)qSize, (void)q;
    Boolean stream = obj == kInStream || obj == kOutStream;
    switch (a->mSelector) {
    case kAudioDevicePropertyNominalSampleRate: {
        if (obj != kDevice) break;
        if (inSize != sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
        Float64 r = *(const Float64*)in;
        if (!IsRate(r)) return kAudioHardwareIllegalOperationError;
        if (r != gRate) AskForRate(r);
        return kAudioHardwareNoError;
    }
    case kAudioStreamPropertyIsActive: {
        if (!stream) break;
        if (inSize != sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
        UInt32 on = *(const UInt32*)in != 0;
        pthread_mutex_lock(&gLock);
        UInt32* flag = obj == kInStream ? &gInActive : &gOutActive;
        Boolean changed = *flag != on;
        *flag = on;
        pthread_mutex_unlock(&gLock);
        if (changed) gHost->PropertiesChanged(gHost, obj, 1, a);
        return kAudioHardwareNoError;
    }
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat: {
        if (!stream) break;
        if (inSize != sizeof(AudioStreamBasicDescription)) return kAudioHardwareBadPropertySizeError;
        const AudioStreamBasicDescription* f = in;
        AudioStreamBasicDescription want = Format(f->mSampleRate);
        if (f->mFormatID != want.mFormatID || f->mChannelsPerFrame != CHANNELS || f->mBitsPerChannel != 32 ||
            (f->mFormatFlags & kAudioFormatFlagIsFloat) == 0 || !IsRate(f->mSampleRate))
            return kAudioDeviceUnsupportedFormatError;
        if (f->mSampleRate != gRate) AskForRate(f->mSampleRate);
        return kAudioHardwareNoError;
    }
    }
    UInt32 size;
    if (Prop(obj, a, 0, NULL, 0, &size, NULL) == kAudioHardwareBadObjectError) return kAudioHardwareBadObjectError;
    return kAudioHardwareUnknownPropertyError;
}

// ---------------------------------------------------------------------------------------------
// The driver itself

static OSStatus Initialize(AudioServerPlugInDriverRef d, AudioServerPlugInHostRef host) {
    (void)d;
    gHost = host;
    pthread_mutex_lock(&gLock);
    SetTicks();
    pthread_mutex_unlock(&gLock);
    return kAudioHardwareNoError;
}

static OSStatus CreateDevice(AudioServerPlugInDriverRef d, CFDictionaryRef desc, const AudioServerPlugInClientInfo* c, AudioObjectID* out) {
    (void)d, (void)desc, (void)c, (void)out;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus DestroyDevice(AudioServerPlugInDriverRef d, AudioObjectID dev) {
    (void)d, (void)dev;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus DeviceClient(AudioServerPlugInDriverRef d, AudioObjectID dev, const AudioServerPlugInClientInfo* c) {
    (void)d, (void)c;
    return dev == kDevice ? kAudioHardwareNoError : kAudioHardwareBadObjectError;
}

static OSStatus PerformConfigChange(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt64 action, void* info) {
    (void)d, (void)info;
    if (dev != kDevice) return kAudioHardwareBadObjectError;
    if (!IsRate((Float64)action)) return kAudioHardwareIllegalOperationError;
    pthread_mutex_lock(&gLock);
    gRate = (Float64)action;
    SetTicks();
    // A new timeline at the new rate (the host stops IO first; this is in case it didn't).
    gAnchor = mach_absolute_time();
    gStamps = 0;
    gWriteEnd = 0;
    memset(gRing, 0, sizeof gRing);
    pthread_mutex_unlock(&gLock);
    // Apps waiting for the new rate (cpal waits for this notification) hear about it.
    AudioObjectPropertyAddress rate = {kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, 0};
    gHost->PropertiesChanged(gHost, kDevice, 1, &rate);
    AudioObjectPropertyAddress formats[] = {
        {kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, 0},
        {kAudioStreamPropertyPhysicalFormat, kAudioObjectPropertyScopeGlobal, 0},
    };
    gHost->PropertiesChanged(gHost, kInStream, 2, formats);
    gHost->PropertiesChanged(gHost, kOutStream, 2, formats);
    return kAudioHardwareNoError;
}

static OSStatus AbortConfigChange(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt64 action, void* info) {
    (void)d, (void)action, (void)info;
    return dev == kDevice ? kAudioHardwareNoError : kAudioHardwareBadObjectError;
}

static OSStatus StartIO(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt32 client) {
    (void)d, (void)client;
    if (dev != kDevice) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gLock);
    if (gClients++ == 0) {
        gAnchor = mach_absolute_time();
        gStamps = 0;
        gWriteEnd = 0;
        memset(gRing, 0, sizeof gRing);
    }
    pthread_mutex_unlock(&gLock);
    return kAudioHardwareNoError;
}

static OSStatus StopIO(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt32 client) {
    (void)d, (void)client;
    if (dev != kDevice) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gLock);
    if (gClients > 0) gClients--;
    pthread_mutex_unlock(&gLock);
    return kAudioHardwareNoError;
}

static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt32 client, Float64* sampleTime, UInt64* hostTime, UInt64* seed) {
    (void)d, (void)client;
    if (dev != kDevice) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gLock);
    Float64 ticksPerRing = gTicksPerFrame * RING;
    if (gAnchor + (UInt64)((Float64)(gStamps + 1) * ticksPerRing) <= mach_absolute_time()) gStamps++;
    *sampleTime = (Float64)gStamps * RING;
    *hostTime = gAnchor + (UInt64)((Float64)gStamps * ticksPerRing);
    *seed = 1;
    pthread_mutex_unlock(&gLock);
    return kAudioHardwareNoError;
}

static OSStatus WillDoIO(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt32 client, UInt32 op, Boolean* willDo, Boolean* inPlace) {
    (void)d, (void)client;
    if (dev != kDevice) return kAudioHardwareBadObjectError;
    *willDo = op == kAudioServerPlugInIOOperationReadInput || op == kAudioServerPlugInIOOperationWriteMix;
    *inPlace = true;
    return kAudioHardwareNoError;
}

static OSStatus BeginEndIO(AudioServerPlugInDriverRef d, AudioObjectID dev, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo* cycle) {
    (void)d, (void)client, (void)op, (void)frames, (void)cycle;
    return dev == kDevice ? kAudioHardwareNoError : kAudioHardwareBadObjectError;
}

static OSStatus DoIO(AudioServerPlugInDriverRef d, AudioObjectID dev, AudioObjectID stream, UInt32 client, UInt32 op, UInt32 frames,
                     const AudioServerPlugInIOCycleInfo* cycle, void* buffer, void* secondary) {
    (void)d, (void)client, (void)secondary;
    if (dev != kDevice) return kAudioHardwareBadObjectError;
    if (op == kAudioServerPlugInIOOperationWriteMix && stream == kOutStream) {
        Float64 t = cycle->mOutputTime.mSampleTime;
        const float* src = buffer;
        for (UInt32 i = 0; i < frames; i++) {
            Float64 s = t + i;
            if (s < 0) continue;
            float* slot = &gRing[((UInt64)s % RING) * CHANNELS];
            memcpy(slot, &src[i * CHANNELS], sizeof(float) * CHANNELS);
        }
        if (t + frames > gWriteEnd) gWriteEnd = t + frames;
    } else if (op == kAudioServerPlugInIOOperationReadInput && stream == kInStream) {
        Float64 t = cycle->mInputTime.mSampleTime;
        float* dst = buffer;
        for (UInt32 i = 0; i < frames; i++) {
            Float64 s = t + i;
            // Only what was written within the last ring's worth; the rest is silence.
            if (s >= 0 && s < gWriteEnd && s >= gWriteEnd - RING)
                memcpy(&dst[i * CHANNELS], &gRing[((UInt64)s % RING) * CHANNELS], sizeof(float) * CHANNELS);
            else
                memset(&dst[i * CHANNELS], 0, sizeof(float) * CHANNELS);
        }
    }
    return kAudioHardwareNoError;
}

// ---------------------------------------------------------------------------------------------
// COM plumbing: Core Audio finds MicClean_Create through Info.plist (CFPlugInFactories).

static HRESULT QueryInterface(void* d, REFIID iid, LPVOID* out);
static ULONG AddRef(void* d);
static ULONG Release(void* d);

static AudioServerPlugInDriverInterface gInterface = {
    NULL,
    QueryInterface,
    AddRef,
    Release,
    Initialize,
    CreateDevice,
    DestroyDevice,
    DeviceClient, // AddDeviceClient
    DeviceClient, // RemoveDeviceClient
    PerformConfigChange,
    AbortConfigChange,
    HasProperty,
    IsPropertySettable,
    GetPropertyDataSize,
    GetPropertyData,
    SetPropertyData,
    StartIO,
    StopIO,
    GetZeroTimeStamp,
    WillDoIO,
    BeginEndIO, // BeginIOOperation
    DoIO,
    BeginEndIO, // EndIOOperation
};
static AudioServerPlugInDriverInterface* gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriver = &gInterfacePtr;

static HRESULT QueryInterface(void* d, REFIID iid, LPVOID* out) {
    (void)d;
    if (!out) return kAudioHardwareIllegalOperationError;
    CFUUIDRef asked = CFUUIDCreateFromUUIDBytes(NULL, iid);
    Boolean ours = CFEqual(asked, IUnknownUUID) || CFEqual(asked, kAudioServerPlugInDriverInterfaceUUID);
    CFRelease(asked);
    if (!ours) return E_NOINTERFACE;
    AddRef(d);
    *out = gDriver;
    return S_OK;
}

static ULONG AddRef(void* d) {
    (void)d;
    pthread_mutex_lock(&gLock);
    ULONG n = ++gRefs;
    pthread_mutex_unlock(&gLock);
    return n;
}

static ULONG Release(void* d) {
    (void)d;
    pthread_mutex_lock(&gLock);
    ULONG n = gRefs > 0 ? --gRefs : 0;
    pthread_mutex_unlock(&gLock);
    return n;
}

__attribute__((visibility("default"))) void* MicClean_Create(CFAllocatorRef allocator, CFUUIDRef type) {
    (void)allocator;
    return CFEqual(type, kAudioServerPlugInTypeUUID) ? gDriver : NULL;
}
