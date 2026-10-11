#include "deviceproperties.h"
#include "directoryscan.h"
#include <libudev.h>
#ifdef QLDM_HAVE_KMOD
#include <libkmod.h>
#endif
#include <QDir>
#include <QFileInfo>
#include <QStringList>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <memory>
#include <sys/stat.h>
#include <unistd.h>
#include <utility>

namespace {
constexpr int textLimit = 4096;
constexpr int efiLimit = 65536;
struct Descriptor {
    int fd = -1;
    explicit Descriptor(int value) : fd(value) {}
    ~Descriptor() { if (fd >= 0) close(fd); }
    Descriptor(const Descriptor &) = delete;
    Descriptor &operator=(const Descriptor &) = delete;
};
Attribute failure(int error)
{
    Attribute a;
    a.error = error;
    a.state = error == EACCES || error == EPERM ? ReadState::PermissionDenied
        : error == ENODEV ? ReadState::Removed
        : error == ENOENT || error == ENODATA ? ReadState::Unavailable : ReadState::Error;
    return a;
}
Attribute available(const QString &value) { return {ReadState::Available, value, 0}; }
Attribute boundedText(const char *value)
{
    if (!value) return {};
    const size_t length = strnlen(value, textLimit + 1);
    return length > textLimit ? failure(EOVERFLOW)
        : available(QString::fromUtf8(value, static_cast<int>(length)));
}
QByteArray readBytes(int fd, int limit, int &error)
{
    QByteArray bytes;
    char buffer[4096];
    error = 0;
    while (bytes.size() <= limit && !QThread::currentThread()->isInterruptionRequested()) {
        const int wanted = qMin(static_cast<int>(sizeof(buffer)), limit + 1 - static_cast<int>(bytes.size()));
        const ssize_t count = read(fd, buffer, wanted);
        if (count < 0) {
            if (errno == EINTR) continue;
            error = errno;
            return {};
        }
        if (count == 0) break;
        bytes.append(buffer, static_cast<int>(count));
    }
    if (QThread::currentThread()->isInterruptionRequested()) { error = ECANCELED; return {}; }
    return bytes;
}
Attribute readText(int directory, const char *name)
{
    Descriptor file(openat(directory, name, O_RDONLY | O_CLOEXEC | O_NOFOLLOW));
    if (file.fd < 0) return failure(errno);
    int error = 0;
    const QByteArray bytes = readBytes(file.fd, textLimit, error);
    if (error) return failure(error);
    if (bytes.size() > textLimit) return failure(EOVERFLOW);
    return available(QString::fromUtf8(bytes).trimmed());
}
Attribute linkTarget(int directory, const char *name)
{
    char buffer[textLimit + 1];
    const ssize_t size = readlinkat(directory, name, buffer, sizeof(buffer));
    if (size < 0) return failure(errno);
    if (size > textLimit) return failure(EOVERFLOW);
    return available(QString::fromUtf8(buffer, static_cast<int>(size)));
}
QString inodeIdentity(const struct stat &info)
{
    return QStringLiteral("%1:%2").arg(static_cast<qulonglong>(info.st_dev))
        .arg(static_cast<qulonglong>(info.st_ino));
}
void put(DeviceProperties &result, const QString &id, const Attribute &value, const QString &source)
{
    result.values.insert(id, value);
    result.sources.insert(id, source);
}
void sysfs(DeviceProperties &result, int fd, const char *key)
{
    const QString name = QLatin1String(key);
    put(result, "sysfs/" + name, readText(fd, key), result.device.path + '/' + name);
}
void usbProperties(DeviceProperties &result, int fd)
{
    const Device &device = result.device;
    if (device.subsystem != "usb" || device.devtype != "usb_device") return;
    UsbProperties &usb = result.usb;
    for (const char *key : {"bConfigurationValue", "bNumConfigurations", "configuration",
                           "bNumInterfaces", "bmAttributes", "bMaxPower"})
        usb.values.insert(QLatin1String(key), readText(fd, key));
    const Attribute configuration = usb.values.value("bConfigurationValue");
    const bool unconfigured = configuration.state == ReadState::Available
        && (configuration.value.isEmpty() || configuration.value == "0");
    if (unconfigured) {
        for (const char *key : {"configuration", "bNumInterfaces", "bmAttributes", "bMaxPower"})
            usb.values.insert(QLatin1String(key), {ReadState::NotApplicable, {}, 0});
        usb.interfaceState = {ReadState::NotApplicable, {}, 0};
    }
    // Only direct USB interface children, never descendant devices behind hubs.
    const DirectoryNames names = unconfigured ? DirectoryNames()
        : boundedDirectoryNames(device.path, 256, {device.sysname + ":*"});
    if (!unconfigured) usb.interfaceState = names.error ? failure(names.error) : available(QString());
    for (const QString &name : names.names) {
        if (QThread::currentThread()->isInterruptionRequested()) {
            usb.interfaces.clear();
            usb.interfaceState = failure(ECANCELED);
            break;
        }
        UsbInterfaceProperties interface;
        interface.name = name;
        interface.path = device.path + '/' + name;
        const QByteArray encoded = name.toUtf8();
        Descriptor child(openat(fd, encoded.constData(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW));
        struct stat before {};
        const int readError = child.fd < 0 ? errno : (fstat(child.fd, &before) != 0 ? errno : 0);
        if (readError) {
            interface.values.insert("state", failure(readError));
        } else {
            const Attribute driver = linkTarget(child.fd, "driver");
            for (const char *key : {"bInterfaceNumber", "bAlternateSetting", "bInterfaceClass",
                                   "bInterfaceSubClass", "bInterfaceProtocol", "interface"})
                interface.values.insert(QLatin1String(key), readText(child.fd, key));
            Attribute binding = driver;
            if (binding.state == ReadState::Available) binding.value = QFileInfo(binding.value).fileName();
            else if (binding.state == ReadState::Unavailable && binding.error == ENOENT) binding = available(QString());
            interface.values.insert("driver", binding);
            const Attribute afterDriver = linkTarget(child.fd, "driver");
            const Attribute afterSetting = readText(child.fd, "bAlternateSetting");
            const Attribute setting = interface.values.value("bAlternateSetting");
            struct stat after {};
            if (fstatat(fd, encoded.constData(), &after, AT_SYMLINK_NOFOLLOW) != 0) {
                const int error = errno;
                interface.values.clear();
                interface.values.insert("state", failure(error));
            } else if (inodeIdentity(before) != inodeIdentity(after)
                || driver.state != afterDriver.state || driver.value != afterDriver.value || driver.error != afterDriver.error
                || setting.state != afterSetting.state || setting.value != afterSetting.value || setting.error != afterSetting.error) {
                interface.values.clear();
                interface.values.insert("state", failure(EAGAIN));
            }
        }
        usb.interfaces.append(interface);
    }
    // A listing failure must not become a successful empty/partial result.
    const Attribute count = usb.values.value("bNumInterfaces");
    bool validCount = false;
    const uint expected = count.value.toUInt(&validCount);
    if (!unconfigured && !names.error && names.names.isEmpty() && count.state != ReadState::Available)
        usb.interfaceState = count;
    if (!unconfigured && !names.error && count.state == ReadState::Available
        && (!validCount || expected > 255 || expected != static_cast<uint>(names.names.size()))) {
        usb.interfaces.clear();
        usb.interfaceState = failure(validCount && expected <= 255 ? EAGAIN : EINVAL);
    }
    // Configuration changes invalidate both its declarations and interface list.
    const Attribute afterConfiguration = readText(fd, "bConfigurationValue");
    if (configuration.state != afterConfiguration.state || configuration.value != afterConfiguration.value
        || configuration.error != afterConfiguration.error) {
        usb.interfaces.clear();
        const Attribute changed = failure(EAGAIN);
        for (auto it = usb.values.begin(); it != usb.values.end(); ++it) it.value() = changed;
        usb.interfaceState = changed;
    }
}

NetworkProperties networkInterface(int fd, const QString &path, const QString &name)
{
    const Attribute index = readText(fd, "ifindex");
    const Attribute type = readText(fd, "type");
    bool valid = false;
    const unsigned int number = index.value.toUInt(&valid);
    NetworkProperties result;
    if (index.state == ReadState::Available && valid && number)
        result = collectNetworkProperties(name, number, type.state == ReadState::Available && type.value == "1");
    else result.addressState = result.permanentAddress = index.state == ReadState::Available ? failure(EINVAL) : index;
    result.name = name;
    result.path = path;
    result.linkType = type;
    if (type.state != ReadState::Available) result.permanentAddress = type;
    result.currentAddress = readText(fd, "address");
    result.assignmentType = readText(fd, "addr_assign_type");
    result.mtu = readText(fd, "mtu");
    const Attribute afterIndex = readText(fd, "ifindex");
    const bool indexChanged = index.state == ReadState::Available
        && (afterIndex.state != ReadState::Available || afterIndex.value != index.value);
    if (indexChanged || result.addressState.state == ReadState::Removed || result.permanentAddress.state == ReadState::Removed) {
        result.addresses.clear();
        const Attribute error = indexChanged
            ? (afterIndex.state != ReadState::Available ? afterIndex : failure(EAGAIN)) : failure(ENODEV);
        result.addressState = result.permanentAddress = result.currentAddress
            = result.assignmentType = result.mtu = error;
    }
    return result;
}
void networkProperties(DeviceProperties &result, int fd)
{
    const Device &device = result.device;
    if (device.subsystem == "net") {
        result.network.append(networkInterface(fd, device.path, device.sysname));
        result.networkState = available(QString());
        return;
    }
    if (device.category != "network") return;
    // A single net interface is represented by its PCI function in the device
    // tree. Read only that function's direct net directory, never ancestor ports.
    Descriptor directory(openat(fd, "net", O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW));
    if (directory.fd < 0) { result.networkState = failure(errno); return; }
    const QString path = device.path + "/net";
    const DirectoryNames entries = boundedDirectoryNames(path, 256);
    if (entries.error) { result.networkState = failure(entries.error); return; }
    result.networkState = available(QString());
    for (const QString &name : entries.names) {
        if (QThread::currentThread()->isInterruptionRequested()) break;
        const QByteArray encoded = name.toUtf8();
        Descriptor interface(openat(directory.fd, encoded.constData(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW));
        if (interface.fd < 0) {
            const Attribute error = failure(errno);
            NetworkProperties unavailable;
            unavailable.name = name;
            unavailable.path = path + '/' + name;
            unavailable.addressState = unavailable.permanentAddress = unavailable.currentAddress
                = unavailable.assignmentType = unavailable.linkType = unavailable.mtu = error;
            result.network.append(unavailable);
        } else result.network.append(networkInterface(interface.fd, path + '/' + name, name));
    }
}
void moduleMetadata(DeviceProperties &result, int driverFd)
{
    Attribute owner = linkTarget(driverFd, "module");
    // ENOENT is absent evidence, not evidence of a built-in driver.
    if (owner.state == ReadState::Available) owner.value = QFileInfo(owner.value).fileName();
    put(result, "module/name", owner, result.values.value("driver/path").value + "/module");
    const QStringList fields {"filename", "version", "description", "author", "license", "firmware"};
    for (const QString &key : fields)
        put(result, "module/" + key, {}, QStringLiteral("installed module metadata"));
    put(result, "module/type", {}, QStringLiteral("kernel module initstate"));
    put(result, "module/runtime_version", {}, QStringLiteral("kernel module version"));
    if (owner.state != ReadState::Available || owner.value.isEmpty()) return;
    Descriptor moduleFd(openat(driverFd, "module", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    if (moduleFd.fd >= 0) {
        put(result, "module/runtime_version", readText(moduleFd.fd, "version"),
            QStringLiteral("/sys/module/%1/version").arg(owner.value));
        const Attribute state = readText(moduleFd.fd, "initstate");
        put(result, "module/type", state.state == ReadState::Available ? Attribute() : state,
            QStringLiteral("/sys/module/%1/initstate").arg(owner.value));
        if (state.state == ReadState::Available
            && (state.value == "live" || state.value == "coming" || state.value == "going"))
            put(result, "module/type", available("modular"), QStringLiteral("/sys/module/%1/initstate").arg(owner.value));
    } else {
        const Attribute error = failure(errno);
        put(result, "module/runtime_version", error, QStringLiteral("kernel module version"));
        put(result, "module/type", error, QStringLiteral("kernel module initstate"));
    }
#ifdef QLDM_HAVE_KMOD
    const char *const noConfiguration[] = {nullptr};
    std::unique_ptr<kmod_ctx, decltype(&kmod_unref)> context(kmod_new(nullptr, noConfiguration), &kmod_unref);
    kmod_module *raw = nullptr;
    const QByteArray name = owner.value.toUtf8();
    const int status = context ? kmod_module_new_from_name_lookup(context.get(), name.constData(), &raw) : -ENOMEM;
    std::unique_ptr<kmod_module, decltype(&kmod_module_unref)> module(raw, &kmod_module_unref);
    if (status < 0 || !module) {
        if (status < 0)
            for (const QString &key : fields) result.values["module/" + key] = failure(-status);
        return;
    }
    const Attribute filename = boundedText(kmod_module_get_path(module.get()));
    put(result, "module/filename", filename, QStringLiteral("libkmod: running-kernel installed module index"));
    kmod_list *info = nullptr;
    const int count = kmod_module_get_info(module.get(), &info);
    if (count < 0) {
        for (const QString &key : fields)
            if (key != "filename") result.values["module/" + key] = failure(-count);
    } else {
        kmod_list *entry = nullptr;
        int examined = 0;
        kmod_list_foreach(entry, info) {
            if (QThread::currentThread()->isInterruptionRequested()) break;
            if (++examined > 4096) {
                for (const QString &key : fields)
                    if (key != "filename") result.values["module/" + key] = failure(EOVERFLOW);
                break;
            }
            const Attribute keyText = boundedText(kmod_module_info_get_key(entry));
            const QString key = keyText.value;
            if (keyText.state != ReadState::Available || key == "filename" || !fields.contains(key)) continue;
            Attribute value = boundedText(kmod_module_info_get_value(entry));
            const Attribute previous = result.values.value("module/" + key);
            if (previous.state == ReadState::Error) continue;
            if (value.state == ReadState::Available && previous.state == ReadState::Available) {
                value.value = previous.value + '\n' + value.value;
                if (value.value.toUtf8().size() > textLimit) value = failure(EOVERFLOW);
            }
            put(result, "module/" + key, value,
                filename.state == ReadState::Available ? filename.value + ": .modinfo/" + key
                                                       : QStringLiteral("libkmod: installed metadata/%1").arg(key));
        }
    }
    kmod_module_info_free_list(info);
#else
    for (const QString &key : fields) result.values["module/" + key].state = ReadState::Unsupported;
#endif
}
}

void PropertiesReader::setRequest(const Device &device, quint64 request)
{
    Q_ASSERT(!isRunning());
    device_ = device;
    request_ = request;
}
DeviceProperties PropertiesReader::takeResult()
{
    Q_ASSERT(!isRunning());
    return std::move(result_);
}
DeviceProperties collectDeviceProperties(const Device &device, bool cachedStorageService)
{
    DeviceProperties result;
    result.device = device;
    const Device &d = result.device;
    const bool efi = d.subsystem == "efivarfs";
    const QByteArray path = d.path.toUtf8();
    if (efi) {
        struct stat entry {};
        if (lstat(path.constData(), &entry) != 0) {
            result.error = errno;
            result.state = errno == ENOENT || errno == ENODEV ? ReadState::Removed : failure(errno).state;
            return result;
        }
        if (inodeIdentity(entry) != d.incarnation) { result.state = ReadState::Removed; return result; }
        if (!S_ISREG(entry.st_mode) || !(entry.st_mode & S_IROTH)) {
            result.state = ReadState::PermissionDenied;
            result.error = EACCES;
            return result;
        }
    }
    Descriptor deviceFd(open(path.constData(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW | (efi ? 0 : O_DIRECTORY)));
    if (deviceFd.fd < 0) {
        result.error = errno;
        result.state = errno == ENOENT || errno == ENODEV ? ReadState::Removed : failure(errno).state;
        return result;
    }
    struct stat before {};
    if (fstat(deviceFd.fd, &before) != 0) {
        result.error = errno; result.state = ReadState::Error; return result;
    }
    const QString identity = inodeIdentity(before);
    const bool sameIdentity = efi ? identity == d.incarnation : d.incarnation.startsWith(identity + ':');
    if (!sameIdentity) { result.state = ReadState::Removed; return result; }
    result.state = ReadState::Available;
    if (efi) {
        if (!S_ISREG(before.st_mode) || !(before.st_mode & S_IROTH)) {
            result.state = ReadState::PermissionDenied;
            result.error = EACCES;
            return result;
        }
        int error = 0;
        result.efiBytes = readBytes(deviceFd.fd, efiLimit, error);
        if (error) put(result, "efi/hex", failure(error), QStringLiteral("efivarfs: complete file bytes"));
        else {
            result.efiTruncated = result.efiBytes.size() > efiLimit;
            result.efiBytes.truncate(efiLimit);
            put(result, "efi/hex", available(QString()), QStringLiteral("efivarfs: complete file bytes, no decoding"));
        }
    } else {
        std::unique_ptr<udev, decltype(&udev_unref)> context(udev_new(), &udev_unref);
        std::unique_ptr<udev_device, decltype(&udev_device_unref)> raw(
            context ? udev_device_new_from_syspath(context.get(), path.constData()) : nullptr, &udev_device_unref);
        if (!raw) {
            result.state = ReadState::Error;
            result.error = context ? EIO : ENOMEM;
            return result;
        }
        const QString stamp = boundedText(udev_device_get_property_value(raw.get(), "USEC_INITIALIZED")).value;
        if (identity + ':' + stamp != d.incarnation) { result.state = ReadState::Removed; return result; }
        // Selected udev metadata only; no unbounded property dump.
        for (const char *key : {"ID_VENDOR_FROM_DATABASE", "ID_VENDOR", "ID_MODEL_FROM_DATABASE", "ID_MODEL",
                "ID_BUS", "ID_PATH", "ID_PATH_TAG", "ID_SERIAL", "ID_SERIAL_SHORT", "ID_REVISION",
                "ID_VENDOR_ID", "ID_MODEL_ID", "PCI_ID", "PCI_SUBSYS_ID", "PCI_CLASS", "PCI_SLOT_NAME",
                "PRODUCT", "PHYS", "MODALIAS", "DEVNAME", "DEVTYPE", "DRIVER", "ID_NET_DRIVER",
                "ID_USB_DRIVER", "ID_FIRMWARE_FROM_DATABASE"}) {
            const Attribute value = boundedText(udev_device_get_property_value(raw.get(), key));
            if (value.state != ReadState::Unavailable)
                put(result, QStringLiteral("udev/") + QLatin1String(key), value, QStringLiteral("udev: %1 on %2").arg(QLatin1String(key), d.path));
        }
        const Attribute driverLink = linkTarget(deviceFd.fd, "driver");
        Attribute driverPath = driverLink;
        if (driverPath.state == ReadState::Available)
            driverPath.value = QDir::cleanPath(driverPath.value.startsWith('/') ? driverPath.value : d.path + '/' + driverPath.value);
        put(result, "driver/path", driverPath, d.path + "/driver");
        Attribute binding = driverLink;
        if (binding.state == ReadState::Available) binding.value = QFileInfo(binding.value).fileName();
        else if (binding.state == ReadState::Unavailable && binding.error == ENOENT) binding = available(QString());
        put(result, "driver/name", binding, d.path + "/driver");
        if (driverLink.state == ReadState::Available) {
            // Follow this one documented kernel link, never a guessed parent driver.
            Descriptor driverFd(openat(deviceFd.fd, "driver", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
            if (driverFd.fd >= 0) {
                const Attribute subsystem = linkTarget(deviceFd.fd, "subsystem");
                put(result, "driver/bus", subsystem.state == ReadState::Available
                    ? available(QFileInfo(subsystem.value).fileName()) : subsystem, d.path + "/subsystem");
                moduleMetadata(result, driverFd.fd);
            } else put(result, "module/name", failure(errno), driverPath.value + "/module");
        }
        QStringList attributes;
        if (d.subsystem == "pci") attributes = {"vendor", "device", "subsystem_vendor", "subsystem_device", "class", "revision", "modalias"};
        else if (d.subsystem == "usb") attributes = {"idVendor", "idProduct", "bcdDevice", "manufacturer", "product", "serial", "authorized", "busnum", "devnum", "version", "speed", "modalias"};
        else if (d.subsystem == "input") attributes = {"name", "phys", "uniq", "modalias"};
        else if (d.subsystem == "nvme") attributes = {"model", "serial", "firmware_rev", "state"};
        else if (d.subsystem == "power_supply") attributes = {"type", "manufacturer", "model_name", "serial_number", "present"};
        else if (d.subsystem == "net") attributes = {"address", "addr_assign_type", "type", "mtu", "ifindex", "operstate", "flags"};
        else if (d.subsystem == "drm" && d.sysname.contains('-')) attributes = {"status", "enabled"};
        else if (d.subsystem == "hdaudio") attributes = {"vendor_name", "chip_name", "vendor_id", "subsystem_id", "revision_id"};
        else if (d.subsystem == "acpi") attributes = {"hid", "uid", "path", "modalias"};
        else if (d.subsystem == "pnp" || d.subsystem == "platform" || d.subsystem == "hid") attributes = {"modalias"};
        else if (d.subsystem == "video4linux") attributes = {"name", "index"};
        else if (d.subsystem == "sound" && d.sysname.startsWith("card")) attributes = {"id"};
        for (const QString &key : attributes) {
            if (QThread::currentThread()->isInterruptionRequested()) break;
            const QByteArray name = key.toLatin1();
            sysfs(result, deviceFd.fd, name.constData());
        }
        if (!QThread::currentThread()->isInterruptionRequested()) result.resources = collectDeviceResources(deviceFd.fd, d);
        if (!QThread::currentThread()->isInterruptionRequested())
            result.storage = collectStorageProperties(d, cachedStorageService);
        if (!QThread::currentThread()->isInterruptionRequested()) networkProperties(result, deviceFd.fd);
        if (cachedStorageService && !QThread::currentThread()->isInterruptionRequested())
            usbProperties(result, deviceFd.fd);
        // Desktop-session queries belong to explicit properties requests, not
        // deep search (which passes cachedStorageService=false).
        if (cachedStorageService && d.subsystem == "drm" && d.sysname.contains('-')
            && !QThread::currentThread()->isInterruptionRequested()) result.display = collectDisplayProperties(d, readText(deviceFd.fd, "status"));
        // Binding can change during a read even if the device instance remains.
        const Attribute afterLink = linkTarget(deviceFd.fd, "driver");
        if (afterLink.state != driverLink.state || afterLink.value != driverLink.value || afterLink.error != driverLink.error) {
            result.state = ReadState::Error;
            result.error = EAGAIN;
        }
    }
    struct stat after {};
    if (lstat(path.constData(), &after) != 0) {
        result.error = errno;
        result.state = errno == ENOENT || errno == ENODEV ? ReadState::Removed : failure(errno).state;
    } else if (inodeIdentity(after) != identity) result.state = ReadState::Removed;
    else if (efi && !(after.st_mode & S_IROTH)) {
        result.state = ReadState::PermissionDenied;
        result.error = EACCES;
        result.efiBytes.clear();
    }
    if (QThread::currentThread()->isInterruptionRequested()) { result.state = ReadState::Error; result.error = ECANCELED; }
    return result;
}

void PropertiesReader::run()
{
    Q_ASSERT(isCurrentThread());
    result_ = collectDeviceProperties(device_);
    result_.request = request_;
}
