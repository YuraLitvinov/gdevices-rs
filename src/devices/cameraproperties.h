#pragma once
#include "device.h"
#include <QJsonObject>

struct CameraProperties {
    bool applicable = false;
    Attribute state;
    QString node;
    // Owned, bounded helper report: each mode retains its queue/format/size/interval relation.
    QJsonObject report;
};
CameraProperties collectCameraProperties(int directory, const Device &device, const QString &node);
// Private headless entry point, before QApplication; never captures or sets a format.
int runCameraQueryHelper(int directory, const QString &path, const QString &node);
