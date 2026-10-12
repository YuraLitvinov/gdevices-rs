#pragma once
#include "device.h"
#include "secureboot.h"
#include <QThread>
#include <QSet>

struct FirmwareMemoryDevice {
    int index = 0; // Firmware record index, not a socket/channel identifier.
    QHash<QString, Attribute> values; // Stable udev property suffixes.
};
struct FirmwareMemoryInformation {
    Attribute status;
    QVector<FirmwareMemoryDevice> devices;
};

struct SystemProperties {
    FirmwareMemoryInformation memoryDevices;
    MokCertificates mok;
    QHash<QString, Attribute> values;
    QHash<QString, QString> sources;
};

// Pure parsers shared with fixtures; no commands or shell evaluation.
Attribute parseCpuInstructionSets(const QString &cpuinfo);
Attribute parseOsName(const QString &text);
FirmwareMemoryInformation parseMemoryDevices(const QHash<QString, QString> &properties);
Attribute parseInstalledMemory(const QHash<QString, QString> &properties);
bool parseCpuList(const QString &text, QSet<int> &cpus);
SystemProperties collectSystemProperties();

class SystemPropertiesReader final : public QThread {
public:
    using QThread::QThread;
    SystemProperties takeResult(); // After finished and wait.
protected:
    void run() override
    {
        Q_ASSERT(isCurrentThread());
        result_ = collectSystemProperties();
    }
private:
    SystemProperties result_;
};
