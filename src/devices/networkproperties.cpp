#include "networkproperties.h"
#include <QByteArray>
#include <QStringList>
#include <QThread>
#include <arpa/inet.h>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <ifaddrs.h>
#include <linux/ethtool.h>
#include <linux/sockios.h>
#include <memory>
#include <net/if.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

namespace {
Attribute failure(int error)
{
    return {error == EACCES || error == EPERM ? ReadState::PermissionDenied
        : error == ENODEV || error == ENXIO ? ReadState::Removed
        : error == EOPNOTSUPP || error == ENODATA ? ReadState::Unavailable : ReadState::Error, {}, error};
}
Attribute ipText(const sockaddr *address)
{
    if (!address) return {};
    const void *bytes = nullptr;
    if (address->sa_family == AF_INET)
        bytes = &reinterpret_cast<const sockaddr_in *>(address)->sin_addr;
    else if (address->sa_family == AF_INET6)
        bytes = &reinterpret_cast<const sockaddr_in6 *>(address)->sin6_addr;
    else return failure(EAFNOSUPPORT);
    char text[INET6_ADDRSTRLEN];
    if (!inet_ntop(address->sa_family, bytes, text, sizeof(text))) return failure(errno);
    return {ReadState::Available, QString::fromLatin1(text), 0};
}
Attribute permanentAddress(const QByteArray &name)
{
    // ETHTOOL_GPERMADDR is a read-only, unprivileged query. Drivers may report
    // all zeroes when no permanent address is known; that is unavailable evidence.
    const int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return failure(errno);
    constexpr unsigned int addressLimit = 32; // Linux MAX_ADDR_LEN.
    std::unique_ptr<ethtool_perm_addr, decltype(&std::free)> query(
        static_cast<ethtool_perm_addr *>(std::calloc(1, sizeof(ethtool_perm_addr) + addressLimit)), &std::free);
    if (!query) { close(fd); return failure(ENOMEM); }
    query->cmd = ETHTOOL_GPERMADDR;
    query->size = addressLimit;
    ifreq request {};
    std::memcpy(request.ifr_name, name.constData(), static_cast<size_t>(name.size()));
    request.ifr_data = reinterpret_cast<char *>(query.get());
    const int status = ioctl(fd, SIOCETHTOOL, &request);
    const int error = errno;
    close(fd);
    if (status < 0) return failure(error);
    // This collector requests MAC addresses only for Ethernet-style interfaces.
    if (query->size != 6) return {};
    bool nonzero = false;
    QStringList bytes;
    for (unsigned int i = 0; i < query->size; ++i) {
        nonzero = nonzero || query->data[i] != 0;
        bytes.append(QStringLiteral("%1").arg(query->data[i], 2, 16, QLatin1Char('0')));
    }
    return nonzero ? Attribute {ReadState::Available, bytes.join(':'), 0} : Attribute {};
}
}

NetworkProperties collectNetworkProperties(const QString &name, unsigned int index, bool ethernet)
{
    NetworkProperties result;
    const QByteArray encoded = name.toUtf8();
    if (encoded.isEmpty() || encoded.size() >= IFNAMSIZ || encoded.contains('\0') || !index) {
        result.addressState = result.permanentAddress = failure(EINVAL);
        return result;
    }
    if (if_nametoindex(encoded.constData()) != index) {
        result.addressState = result.permanentAddress = failure(ENODEV);
        return result;
    }
    ifaddrs *raw = nullptr;
    if (getifaddrs(&raw) != 0) result.addressState = failure(errno);
    else {
        std::unique_ptr<ifaddrs, decltype(&freeifaddrs)> addresses(raw, &freeifaddrs);
        result.addressState = {ReadState::Available, {}, 0};
        for (const ifaddrs *entry = addresses.get(); entry; entry = entry->ifa_next) {
            if (QThread::currentThread()->isInterruptionRequested()) {
                result.addresses.clear();
                result.addressState = failure(ECANCELED);
                break;
            }
            if (!entry->ifa_name || !entry->ifa_addr) continue;
            // getifaddrs may use the IPv4 address label (e.g. eth0:1) instead
            // of the base interface name. Include these secondary addresses.
            const size_t length = static_cast<size_t>(encoded.size());
            if (std::strcmp(entry->ifa_name, encoded.constData()) != 0
                && !(std::strncmp(entry->ifa_name, encoded.constData(), length) == 0
                     && entry->ifa_name[length] == ':')) continue;
            const int family = entry->ifa_addr->sa_family;
            if (family != AF_INET && family != AF_INET6) continue;
            const Attribute address = ipText(entry->ifa_addr);
            if (address.state != ReadState::Available) {
                result.addresses.clear();
                result.addressState = address;
                break;
            }
            QString text = address.value;
            if (family == AF_INET6) {
                const unsigned int scope = reinterpret_cast<const sockaddr_in6 *>(entry->ifa_addr)->sin6_scope_id;
                if (scope) text += '%' + QString::number(scope);
            }
            const Attribute mask = entry->ifa_netmask && entry->ifa_netmask->sa_family == family
                ? ipText(entry->ifa_netmask) : Attribute {};
            result.addresses.append({family == AF_INET ? QStringLiteral("IPv4") : QStringLiteral("IPv6"), text, mask});
        }
    }
    if (!QThread::currentThread()->isInterruptionRequested())
        result.permanentAddress = ethernet ? permanentAddress(encoded)
            : Attribute {ReadState::NotApplicable, {}, 0};
    if (if_nametoindex(encoded.constData()) != index) {
        result.addresses.clear();
        result.addressState = result.permanentAddress = failure(ENODEV);
    }
    return result;
}
