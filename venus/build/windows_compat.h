/*
 * Copyright 2026 WinBridge contributors
 * SPDX-License-Identifier: MIT
 */
#ifndef WINBRIDGE_WINDOWS_COMPAT_H
#define WINBRIDGE_WINDOWS_COMPAT_H

#include <intrin.h>

static __forceinline long
_interlockedadd(long volatile *addend, long value)
{
   return _InterlockedExchangeAdd(addend, value) + value;
}

static __forceinline __int64
_interlockedadd64(__int64 volatile *addend, __int64 value)
{
   return _InterlockedExchangeAdd64(addend, value) + value;
}

static __forceinline __int64
_interlockedincrement64(__int64 volatile *addend)
{
   return _InterlockedIncrement64(addend);
}

static __forceinline __int64
_interlockeddecrement64(__int64 volatile *addend)
{
   return _InterlockedDecrement64(addend);
}

static __forceinline __int64
_interlockedexchangeadd64(__int64 volatile *target, __int64 value)
{
   return _InterlockedExchangeAdd64(target, value);
}

static __forceinline __int64
_interlockedexchange64(__int64 volatile *target, __int64 value)
{
   return _InterlockedExchange64(target, value);
}

#endif
