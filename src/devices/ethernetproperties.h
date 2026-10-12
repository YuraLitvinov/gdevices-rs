#pragma once

#include "device.h"

// Owned read-only ethtool snapshot for one Ethernet-style, non-wireless interface.
struct EthernetProperties {
    bool applicable = false;
    Attribute scope {ReadState::NotApplicable, {}, 0};
    Attribute carrier;
    Attribute speed; // Mbps, not measured throughput.
    Attribute duplex; // Linux DUPLEX_* value.
    Attribute autoneg; // Linux AUTONEG_* value.
    Attribute supportedModes; // Kernel-provided bit names, one per line.
    Attribute partnerModes;
    QString source;

    void invalidate(const Attribute &reason)
    {
        scope = carrier = speed = duplex = autoneg = supportedModes = partnerModes = reason;
    }
};

EthernetProperties collectEthernetProperties(const QString &name, unsigned int index);
