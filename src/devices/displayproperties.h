#pragma once
#include "device.h"

// Owned desktop-session snapshot. All numeric strings use the C locale.
struct DisplayProperties {
    QString backend;
    QString output;
    Attribute status;
    QHash<QString, Attribute> values;
    QHash<QString, QString> sources;
};

DisplayProperties collectDisplayProperties(const Device &device);
