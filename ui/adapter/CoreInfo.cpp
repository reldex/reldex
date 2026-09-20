#include "CoreInfo.h"

#include <reldex.h>

CoreInfo::CoreInfo(QObject *parent)
    : QObject(parent)
    , m_abiVersion(reldex_abi_version())
{
}

quint32 CoreInfo::abiVersion() const
{
    return m_abiVersion;
}
