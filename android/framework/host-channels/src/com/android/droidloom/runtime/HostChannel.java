/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.droidloom.runtime;

import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.ParcelFileDescriptor;
import android.system.Os;
import android.system.OsConstants;
import android.system.StructTimeval;
import java.io.FileDescriptor;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/** SDK-only client of system_server's authenticated, setup-only FD delegation. */
public final class HostChannel implements AutoCloseable {
    public static final String BUILD_ABI = "DROIDLOOM_HOST_CHANNELS_ABI=1;";
    public static final int TEXT_INPUT = 1, CLIPBOARD = 2, NOTIFICATIONS = 3, VOLUME = 4,
            BRIGHTNESS = 5;
    private final ParcelFileDescriptor descriptor;
    private final InputStream input;
    private final OutputStream output;

    private HostChannel(ParcelFileDescriptor descriptor) throws IOException {
        this.descriptor = descriptor;
        input = new ParcelFileDescriptor.AutoCloseInputStream(descriptor);
        try {
            output = new ParcelFileDescriptor.AutoCloseOutputStream(
                    ParcelFileDescriptor.dup(descriptor.getFileDescriptor()));
        } catch (IOException error) { input.close(); throw error; }
    }
    public static HostChannel open(int channel) throws IOException {
        try (LocalSocket setup = new LocalSocket()) {
            setup.connect(new LocalSocketAddress("droidloom-host-channels"));
            setup.setSoTimeout(5000);
            if (setup.getPeerCredentials().getUid() != 1000)
                throw new IOException("Untrusted framework channel owner");
            setup.getOutputStream().write(new byte[] {1, (byte) channel});
            int result = setup.getInputStream().read();
            FileDescriptor[] descriptors = setup.getAncillaryFileDescriptors();
            try {
                if (result != 1 || descriptors == null || descriptors.length != 1)
                    throw new IOException("Framework channel delegation rejected");
                return new HostChannel(ParcelFileDescriptor.dup(descriptors[0]));
            } finally {
                if (descriptors != null) for (FileDescriptor fd : descriptors)
                    try { Os.close(fd); } catch (Exception ignored) { }
            }
        }
    }
    public InputStream getInputStream() { return input; }
    public OutputStream getOutputStream() { return output; }
    public void setSoTimeout(int millis) throws IOException {
        try {
            Os.setsockoptTimeval(descriptor.getFileDescriptor(), OsConstants.SOL_SOCKET,
                    OsConstants.SO_RCVTIMEO, StructTimeval.fromMillis(millis));
        } catch (Exception error) { throw new IOException(error); }
    }
    public void shutdownInput() { shutdown(OsConstants.SHUT_RD); }
    public void shutdownOutput() { shutdown(OsConstants.SHUT_WR); }
    private void shutdown(int direction) {
        try { Os.shutdown(descriptor.getFileDescriptor(), direction); }
        catch (Exception ignored) { }
    }
    @Override public void close() throws IOException {
        shutdown(OsConstants.SHUT_RDWR);
        try { input.close(); } finally { output.close(); }
    }
}
