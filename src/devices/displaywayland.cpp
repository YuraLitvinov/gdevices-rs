#include "displaybackend.h"
#include <QElapsedTimer>
#include <QThread>
#include <cerrno>
#include <cstring>
#include <memory>
#include <vector>
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <wayland-client.h>

namespace DisplayBackend {
namespace {
// Private xdg-output v3 wire descriptors. Only this read-only protocol is bound;
// using runtime core interfaces avoids generated code's link-time Wayland symbols.
const wl_message xdgRequests[] {{"destroy", "", nullptr}};
const wl_message xdgEvents[] {{"logical_position", "ii", nullptr}, {"logical_size", "ii", nullptr},
    {"done", "", nullptr}, {"name", "2s", nullptr}, {"description", "2s", nullptr}};
const wl_interface xdgInterface {"zxdg_output_v1", 3, 1, xdgRequests, 5, xdgEvents};
struct XdgListener {
    void (*position)(void *, wl_proxy *, int32_t, int32_t);
    void (*size)(void *, wl_proxy *, int32_t, int32_t);
    void (*done)(void *, wl_proxy *);
    void (*name)(void *, wl_proxy *, const char *);
    void (*description)(void *, wl_proxy *, const char *);
};
struct Output {
    uint32_t id = 0;
    wl_proxy *proxy = nullptr;
    wl_proxy *xdg = nullptr;
    QString name;
    bool nameConflict = false;
    bool removed = false;
    bool modeKnown = false;
    int width = 0, height = 0, refresh = 0;
    int transform = -1, scale = 1, logicalWidth = 0, logicalHeight = 0;
};
void outputName(Output &output, const char *name)
{
    if (!name || strnlen(name, textLimit + 1) > textLimit) { output.nameConflict = true; return; }
    const QString value = QString::fromUtf8(name);
    if (!output.name.isEmpty() && output.name != value) output.nameConflict = true;
    output.name = value;
}
const wl_output_listener outputListener {
    [](void *data, wl_output *, int32_t, int32_t, int32_t, int32_t, int32_t, const char *, const char *, int32_t transform) {
        static_cast<Output *>(data)->transform = transform;
    },
    [](void *data, wl_output *, uint32_t flags, int32_t width, int32_t height, int32_t refresh) {
        if (!(flags & WL_OUTPUT_MODE_CURRENT)) return;
        auto &output = *static_cast<Output *>(data);
        output.modeKnown = true;
        output.width = width; output.height = height; output.refresh = refresh;
    },
    [](void *, wl_output *) {},
    [](void *data, wl_output *, int32_t scale) { static_cast<Output *>(data)->scale = scale; },
    [](void *data, wl_output *, const char *name) { outputName(*static_cast<Output *>(data), name); },
    [](void *, wl_output *, const char *) {}
};
const XdgListener xdgListener {
    [](void *, wl_proxy *, int32_t, int32_t) {},
    [](void *data, wl_proxy *, int32_t width, int32_t height) {
        auto &output = *static_cast<Output *>(data);
        output.logicalWidth = width; output.logicalHeight = height;
    },
    [](void *, wl_proxy *) {},
    [](void *data, wl_proxy *, const char *name) { outputName(*static_cast<Output *>(data), name); },
    [](void *, wl_proxy *, const char *) {}
};
#define WAYLAND_FUNCTIONS(X) \
    X(connectFd, wl_display_connect_to_fd) X(disconnect, wl_display_disconnect) \
    X(getFd, wl_display_get_fd) X(flush, wl_display_flush) \
    X(dispatch, wl_display_dispatch_pending) X(prepare, wl_display_prepare_read) \
    X(cancel, wl_display_cancel_read) X(readEvents, wl_display_read_events) \
    X(marshal, wl_proxy_marshal_flags) X(addListener, wl_proxy_add_listener) \
    X(version, wl_proxy_get_version) X(destroy, wl_proxy_destroy)
struct Wayland {
    QLibrary library {QStringLiteral("libwayland-client.so.0")};
    wl_display *display = nullptr;
    wl_proxy *registry = nullptr, *manager = nullptr;
    const wl_interface *registryInterface = nullptr, *outputInterface = nullptr, *callbackInterface = nullptr;
    const wl_interface *getOutputTypes[2] {&xdgInterface, nullptr};
    wl_message managerRequests[2] {{"destroy", "", nullptr}, {"get_xdg_output", "no", getOutputTypes}};
    wl_interface managerInterface {"zxdg_output_manager_v1", 3, 2, managerRequests, 0, nullptr};
    std::vector<std::unique_ptr<Output>> outputs;
    QElapsedTimer timer;
    int errorNumber = 0;
#define DECLARE(member, symbol) decltype(&symbol) member = nullptr;
    WAYLAND_FUNCTIONS(DECLARE)
#undef DECLARE
    ~Wayland()
    {
        for (const auto &output : outputs) {
            if (output->xdg) destroy(output->xdg);
            if (output->proxy) destroy(output->proxy);
        }
        if (manager) destroy(manager);
        if (registry) destroy(registry);
        if (display) disconnect(display);
        library.unload();
    }
    bool load()
    {
        if (!library.load()) return false;
        bool valid = true;
#define LOAD(member, symbol) valid = resolve(library, #symbol, member) && valid;
        WAYLAND_FUNCTIONS(LOAD)
#undef LOAD
        registryInterface = reinterpret_cast<const wl_interface *>(library.resolve("wl_registry_interface"));
        outputInterface = reinterpret_cast<const wl_interface *>(library.resolve("wl_output_interface"));
        callbackInterface = reinterpret_cast<const wl_interface *>(library.resolve("wl_callback_interface"));
        getOutputTypes[1] = outputInterface;
        return valid && registryInterface && outputInterface && callbackInterface;
    }
    bool connect()
    {
        // Open our own socket; never consume WAYLAND_SOCKET, which may already
        // belong to Qt. This also makes connection establishment cancellable.
        QString name = QString::fromLocal8Bit(qgetenv("WAYLAND_DISPLAY"));
        if (name.isEmpty()) name = "wayland-0";
        if (!name.startsWith('/')) {
            const QString directory = QString::fromLocal8Bit(qgetenv("XDG_RUNTIME_DIR"));
            if (directory.isEmpty()) { errorNumber = ENOENT; return false; }
            name = directory + '/' + name;
        }
        const QByteArray path = name.toLocal8Bit();
        sockaddr_un address {};
        if (path.size() >= static_cast<int>(sizeof(address.sun_path))) { errorNumber = ENAMETOOLONG; return false; }
        address.sun_family = AF_UNIX;
        std::memcpy(address.sun_path, path.constData(), static_cast<size_t>(path.size()));
        const int socket = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
        if (socket < 0) { errorNumber = errno; return false; }
        int status = ::connect(socket, reinterpret_cast<const sockaddr *>(&address), sizeof(address));
        if (status < 0 && errno == EINPROGRESS) {
            status = 0;
            while (!QThread::currentThread()->isInterruptionRequested() && timer.elapsed() < queryTimeoutMs) {
                pollfd event {socket, POLLOUT, 0};
                const int ready = poll(&event, 1, 100);
                if (ready < 0 && errno == EINTR) continue;
                if (ready < 0) { errorNumber = errno; break; }
                if (!ready) continue;
                int errorValue = 0;
                socklen_t size = sizeof(errorValue);
                if (getsockopt(socket, SOL_SOCKET, SO_ERROR, &errorValue, &size) != 0) errorValue = errno;
                errorNumber = errorValue;
                status = errorValue ? -1 : 1;
                break;
            }
            if (!status) errorNumber = QThread::currentThread()->isInterruptionRequested() ? ECANCELED : ETIMEDOUT;
        } else if (status < 0) errorNumber = errno;
        if (errorNumber) { close(socket); return false; }
        display = connectFd(socket); // Takes ownership, including on failure.
        if (!display) { errorNumber = errno ? errno : EIO; return false; }
        return true;
    }
    wl_proxy *bind(uint32_t id, const wl_interface *interface, uint32_t serverVersion)
    {
        const uint32_t requested = qMin(serverVersion, static_cast<uint32_t>(interface->version));
        return marshal(registry, WL_REGISTRY_BIND, interface, requested, 0, id, interface->name, requested, nullptr);
    }
    bool sync()
    {
        bool done = false;
        auto *callback = marshal(reinterpret_cast<wl_proxy *>(display), WL_DISPLAY_SYNC, callbackInterface, 1, 0, nullptr);
        if (!callback) { errorNumber = ENOMEM; return false; }
        static const wl_callback_listener listener {[](void *data, wl_callback *, uint32_t) { *static_cast<bool *>(data) = true; }};
        if (addListener(callback, reinterpret_cast<void (**)(void)>(const_cast<wl_callback_listener *>(&listener)), &done) < 0) errorNumber = EIO;
        while (!done && !errorNumber) {
            if (QThread::currentThread()->isInterruptionRequested()) { errorNumber = ECANCELED; break; }
            const int remaining = queryTimeoutMs - static_cast<int>(timer.elapsed());
            if (remaining <= 0) { errorNumber = ETIMEDOUT; break; }
            if (dispatch(display) < 0) { errorNumber = EIO; break; }
            if (done) break;
            if (prepare(display) != 0) continue;
            const int sent = flush(display);
            const bool pendingWrite = sent < 0 && errno == EAGAIN;
            if (sent < 0 && !pendingWrite) { cancel(display); errorNumber = EIO; break; }
            pollfd event {getFd(display), static_cast<short>(POLLIN | (pendingWrite ? POLLOUT : 0)), 0};
            const int ready = poll(&event, 1, qMin(remaining, 100));
            if (ready < 0) {
                const int errorValue = errno;
                cancel(display);
                if (errorValue != EINTR) errorNumber = errorValue;
            } else if (event.revents & (POLLERR | POLLHUP | POLLNVAL)) { cancel(display); errorNumber = EIO; }
            else if (event.revents & POLLIN) { if (readEvents(display) < 0) errorNumber = EIO; }
            else cancel(display);
        }
        destroy(callback);
        return !errorNumber;
    }
};
#undef WAYLAND_FUNCTIONS
const wl_registry_listener registryListener {
    [](void *data, wl_registry *, uint32_t id, const char *name, uint32_t version) {
        auto &api = *static_cast<Wayland *>(data);
        if (std::strcmp(name, "wl_output") == 0) {
            if (api.outputs.size() >= static_cast<size_t>(outputLimit)) { api.errorNumber = EOVERFLOW; return; }
            auto output = std::make_unique<Output>();
            output->id = id;
            output->proxy = api.bind(id, api.outputInterface, qMin(version, 4u));
            if (!output->proxy) { api.errorNumber = ENOMEM; return; }
            if (api.addListener(output->proxy, reinterpret_cast<void (**)(void)>(const_cast<wl_output_listener *>(&outputListener)), output.get()) < 0)
                api.errorNumber = EIO;
            api.outputs.push_back(std::move(output));
        } else if (std::strcmp(name, "zxdg_output_manager_v1") == 0 && !api.manager) {
            api.manager = api.bind(id, &api.managerInterface, qMin(version, 3u));
            if (!api.manager) api.errorNumber = ENOMEM;
        }
    },
    [](void *data, wl_registry *, uint32_t id) {
        for (const auto &output : static_cast<Wayland *>(data)->outputs)
            if (output->id == id) output->removed = true;
    }
};
}

DisplayProperties wayland(const QString &connector)
{
    DisplayProperties result;
    result.backend = "Wayland";
    Wayland api;
    if (!api.load()) return result;
    api.timer.start();
    if (!api.connect()) { result.status = error(api.errorNumber); return result; }
    api.registry = api.marshal(reinterpret_cast<wl_proxy *>(api.display), WL_DISPLAY_GET_REGISTRY, api.registryInterface, 1, 0, nullptr);
    if (!api.registry) { result.status = error(ENOMEM); return result; }
    if (api.addListener(api.registry, reinterpret_cast<void (**)(void)>(const_cast<wl_registry_listener *>(&registryListener)), &api) < 0) {
        result.status = error(EIO); return result;
    }
    if (!api.sync()) { result.status = error(api.errorNumber); return result; }
    if (api.manager) {
        for (const auto &output : api.outputs) {
            if (output->removed) continue;
            output->xdg = api.marshal(api.manager, 1, &xdgInterface, api.version(api.manager), 0, nullptr, output->proxy);
            if (!output->xdg) { result.status = error(ENOMEM); return result; }
            if (api.addListener(output->xdg, reinterpret_cast<void (**)(void)>(const_cast<XdgListener *>(&xdgListener)), output.get()) < 0) {
                result.status = error(EIO); return result;
            }
        }
    }
    if (!api.sync()) { result.status = error(api.errorNumber); return result; }
    const Output *selected = nullptr;
    for (const auto &output : api.outputs) {
        if (output->removed || output->nameConflict || canonicalName(output->name) != canonicalName(connector)) continue;
        if (selected) { result.status = error(ENOTUNIQ); return result; }
        selected = output.get();
    }
    if (!selected) return result;
    result.output = selected->name;
    result.status = available("exposed");
    const auto put = [&](const QString &key, const QString &value, const QString &source) {
        result.values.insert(key, available(value)); result.sources.insert(key, source);
    };
    result.sources.insert("depth", "Wayland output protocols do not expose desktop color depth");
    if (selected->modeKnown && selected->width > 0 && selected->height > 0) {
        put("resolution", QStringLiteral("%1x%2").arg(selected->width).arg(selected->height), "wl_output current mode, before rotation/scaling");
        if (selected->refresh > 0) put("refresh", number(selected->refresh / 1000.0), "wl_output current mode; nominal rate, not measured/VRR rate");
    }
    if (selected->transform >= 0 && selected->transform <= 7) {
        put("rotation", QString::number((selected->transform % 4) * 90), "wl_output geometry transform, counterclockwise");
        put("reflection", selected->transform >= 4 ? "vertical" : "none", "wl_output transform: reflection about the vertical axis, then rotation");
    }
    if (selected->logicalWidth > 0 && selected->logicalHeight > 0 && selected->modeKnown
        && selected->width > 0 && selected->height > 0 && selected->transform >= 0 && selected->transform <= 7) {
        const bool swapped = selected->transform % 2;
        const double x = static_cast<double>(swapped ? selected->height : selected->width) / selected->logicalWidth;
        const double y = static_cast<double>(swapped ? selected->width : selected->height) / selected->logicalHeight;
        put("scale", number(x) + ' ' + number(y), "Wayland mode/logical-size ratio (X, Y), adjusted for rotation; approximate when logical sizes are rounded");
        put("scale_kind", "derived", "xdg-output logical_size");
    } else if (selected->scale > 0) {
        put("scale", number(selected->scale) + ' ' + number(selected->scale), "wl_output integer buffer scale; fractional desktop scale unavailable");
        put("scale_kind", "integer", "wl_output scale");
    }
    return result;
}
}
