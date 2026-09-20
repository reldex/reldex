#pragma once

#include <QObject>
#include <QtQml/qqmlregistration.h>

// A single, deliberately tiny QML-visible class for M1.5: it proves the
// reldex-ffi -> C++ adapter -> QML path exists at all. No hub, no session,
// no business logic here -- that is M1.6 (ADR-0003 D1's
// ReldexApp/SessionController/ResultTableModel), owned separately.
class CoreInfo : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    QML_SINGLETON

    // The packed ABI version reldex_abi_version() reports: (major << 16) |
    // minor. Deliberately not decoded into major/minor here -- that would be
    // the first sliver of adapter logic beyond "call the FFI and expose the
    // result", which is exactly what this class must not grow yet.
    Q_PROPERTY(quint32 abiVersion READ abiVersion CONSTANT)

public:
    explicit CoreInfo(QObject *parent = nullptr);

    [[nodiscard]] quint32 abiVersion() const;

private:
    quint32 m_abiVersion;
};
