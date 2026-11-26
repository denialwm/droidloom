/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.policy;

import android.content.Context;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.Binder;
import android.os.Handler;
import android.os.IBinder;
import android.os.Parcel;
import android.os.PowerManager;
import android.os.Process;
import android.os.ServiceManager;
import android.os.SystemClock;
import android.os.SystemProperties;
import android.system.Os;
import android.util.Slog;
import java.io.FileDescriptor;
import java.io.IOException;

/** Framework startup/power hooks and authenticated socket delegation; never relays payloads. */
final class DroidloomHostChannels {
    static final String BUILD_ABI = "DROIDLOOM_HOST_CHANNELS_ABI=1;";
    static final String NATIVE_ABI = "DROIDLOOM_NATIVE_INPUT_ABI=2;";
    private static final String POWER_TOKEN = "com.android.droidloom.input.PowerCallback";

    static void start(Context context, Handler policyHandler) {
        SystemProperties.set("sys.droidloom.framework_ready", "0");
        final LocalServerSocket listener;
        try { listener = new LocalServerSocket("droidloom-host-channels"); }
        catch (IOException error) { throw new IllegalStateException("Host channel listener", error); }
        Binder power = new Binder() {
            @Override protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) {
                if (code != IBinder.FIRST_CALL_TRANSACTION) return false;
                if (Binder.getCallingUid() != Process.SYSTEM_UID) throw new SecurityException();
                data.enforceInterface(POWER_TOKEN);
                final boolean powered = data.readBoolean();
                data.enforceNoDataAvail();
                // Native input may hold its state lock: effects run on the existing policy handler.
                policyHandler.post(() -> {
                    PowerManager manager = context.getSystemService(PowerManager.class);
                    if (manager.isInteractive() == powered) return;
                    long now = SystemClock.uptimeMillis();
                    if (powered) manager.wakeUp(now, 1, "droidloom:host-display");
                    else manager.goToSleep(now, 4, 0);
                });
                return true;
            }
        };
        Parcel request = Parcel.obtain(), reply = Parcel.obtain();
        try {
            IBinder input = ServiceManager.getService("inputflinger");
            if (input == null) throw new IllegalStateException("InputFlinger unavailable");
            request.writeInterfaceToken("android.os.IInputFlinger");
            request.writeInt(2);
            request.writeStrongBinder(power);
            if (!input.transact(0x00444c03, request, reply, 0)) throw new IOException("Native input setup unsupported");
            reply.readException();
            if (!reply.readBoolean()) throw new IOException("Native input listener failed");
        } catch (Exception error) {
            try { listener.close(); } catch (IOException ignored) { }
            throw new IllegalStateException(NATIVE_ABI + " setup failed", error);
        } finally { request.recycle(); reply.recycle(); }
        Thread worker = new Thread(() -> serve(context, listener), "DroidloomChannels");
        worker.setDaemon(true);
        worker.start();
        com.android.server.wm.DroidloomFrameworkControl.start();
        Slog.i("DroidloomChannels", BUILD_ABI + NATIVE_ABI + " native listener and FD delegation ready");
    }

    private static void serve(Context context, LocalServerSocket listener) {
        while (true) {
            try (LocalSocket client = listener.accept()) {
                client.setSoTimeout(1000);
                int uid = client.getPeerCredentials().getUid();
                // Check credentials before accepting any bytes from an untrusted process.
                int ime = packageUid(context, "com.android.droidloom.ime");
                int systemui = packageUid(context, "com.android.systemui");
                if (uid != ime && uid != systemui) continue;
                int version = client.getInputStream().read();
                int channel = client.getInputStream().read();
                String name;
                if (version != 1) continue;
                if (channel == 1 && uid == ime) name = "text-input";
                else if (channel == 2 && uid == systemui) name = "clipboard";
                else if (channel == 3 && uid == systemui) name = "notifications";
                else if (channel == 4 && uid == systemui) name = "volume";
                else if (channel == 5 && uid == systemui) name = "brightness";
                else continue;
                String path = "/dev/socket/droidloom-framework-host/" + name;
                // The device brokers keep record boundaries; the presenter streams.
                int type = channel >= 4 ? LocalSocket.SOCKET_SEQPACKET : LocalSocket.SOCKET_STREAM;
                try (LocalSocket host = new LocalSocket(type)) {
                    host.connect(new LocalSocketAddress(path, LocalSocketAddress.Namespace.FILESYSTEM));
                    if (host.getPeerCredentials().getUid() != Os.stat(path).st_uid)
                        throw new SecurityException("Untrusted host channel owner");
                    client.setFileDescriptorsForSend(new FileDescriptor[] {host.getFileDescriptor()});
                    client.getOutputStream().write(1);
                    client.setFileDescriptorsForSend(null);
                }
            } catch (Exception error) {
                Slog.w("DroidloomChannels", "Channel setup failed: " + error.getClass().getSimpleName());
            }
        }
    }
    private static int packageUid(Context context, String name) {
        try { return context.getPackageManager().getPackageUidAsUser(name, 0); }
        catch (android.content.pm.PackageManager.NameNotFoundException error) { return -1; }
    }
    private DroidloomHostChannels() {}
}
