/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.notifications;

import android.app.ActivityOptions;
import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import com.android.droidloom.runtime.HostChannel;
import android.os.Bundle;
import android.os.Handler;
import android.os.UserHandle;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;
import android.util.Log;
import android.util.LruCache;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.UUID;
import org.json.JSONArray;
import org.json.JSONObject;

/** Event-driven listener; PendingIntents stay inside Android and never cross the socket. */
public final class NotificationBridge extends NotificationListenerService implements AutoCloseable {
    private static final String TAG = "DroidloomNotifications";
    private static final int MAX_ACTIVE = 256;
    // Room for a 96 px large icon in hex next to the longest texts.
    private static final int MAX_FRAME = 262144;
    private final Context context;
    private final Handler main;
    private final Object lock = new Object();
    private final LinkedHashMap<String, Entry> active = new LinkedHashMap<>();
    // Keys whose latest post should alert and has not reached the host yet.
    private final HashSet<String> alerting = new HashSet<>();
    private final LruCache<String, String> icons = new LruCache<>(32);
    private final LruCache<String, String> components = new LruCache<>(32);
    private final String session = UUID.randomUUID().toString();
    private long generation;
    private boolean dirty;
    private volatile boolean stopped, connected, listening;
    private volatile HostChannel socket;
    private static final class Entry {
        final StatusBarNotification sbn;
        final String version;
        final int importance;
        Entry(StatusBarNotification sbn, String version, int importance) {
            this.sbn = sbn; this.version = version; this.importance = importance;
        }
    }
    public NotificationBridge(Context context) { this.context=context; main=new Handler(context.getMainLooper()); }
    public void start() {
        try {
            registerAsSystemService(context, new ComponentName(context, NotificationBridge.class), UserHandle.USER_SYSTEM);
            Log.i(TAG, "DROIDLOOM_NOTIFICATIONS_ABI=3;");
            Thread worker = new Thread(this::connect, "droidloom-notifications");
            worker.setDaemon(true); worker.start();
        } catch (Exception e) { Log.e(TAG, "Notification listener registration failed: " + e.getClass().getSimpleName()); }
    }
    @Override public void onListenerConnected() {
        synchronized (lock) {
            active.clear(); alerting.clear(); listening=true;
            StatusBarNotification[] all=getActiveNotifications();
            if (all!=null) for (StatusBarNotification sbn:all) put(sbn, getCurrentRanking(), false);
            dirty=true; lock.notifyAll();
        }
    }
    @Override public void onListenerDisconnected() {
        listening=false;
        disconnect();
    }
    @Override public void onNotificationPosted(StatusBarNotification sbn, RankingMap ranking) {
        synchronized(lock) { put(sbn,ranking,true); dirty=true; lock.notifyAll(); }
    }
    @Override public void onNotificationRemoved(StatusBarNotification sbn) {
        synchronized(lock) { active.remove(sbn.getKey()); alerting.remove(sbn.getKey()); dirty=true; lock.notifyAll(); }
    }
    private void put(StatusBarNotification sbn, RankingMap map, boolean live) {
        Ranking rank = new Ranking();
        boolean ranked = map!=null && map.getRanking(sbn.getKey(),rank);
        int importance=ranked?rank.getImportance():NotificationManager.IMPORTANCE_DEFAULT;
        if (ranked && (rank.isSuspended() || importance==NotificationManager.IMPORTANCE_NONE)) {
            active.remove(sbn.getKey()); alerting.remove(sbn.getKey()); return;
        }
        boolean update=active.containsKey(sbn.getKey());
        if (!update && active.size()>=MAX_ACTIVE) { String oldest=active.keySet().iterator().next(); active.remove(oldest); alerting.remove(oldest); }
        active.put(sbn.getKey(),new Entry(sbn,session+"-"+(++generation),importance));
        if (live && alerts(sbn,rank,ranked,importance,update)) alerting.add(sbn.getKey());
    }
    /** SystemUI's heads-up decision: HIGH importance, and the post says something new. */
    private static boolean alerts(StatusBarNotification sbn, Ranking rank, boolean ranked, int importance, boolean update) {
        Notification n=sbn.getNotification();
        if (importance<NotificationManager.IMPORTANCE_HIGH || n.isGroupSummary() || n.suppressAlertingDueToGrouping()) return false;
        if (update && (n.flags & Notification.FLAG_ONLY_ALERT_ONCE)!=0) return false;
        return !ranked || ((rank.getSuppressedVisualEffects() & NotificationManager.Policy.SUPPRESSED_EFFECT_PEEK)==0 && rank.isTextChanged());
    }
    private void connect() {
        com.android.droidloom.runtime.CpuPlacement.background();
        while (!stopped) {
            try (HostChannel current = HostChannel.open(HostChannel.NOTIFICATIONS)) {
                socket=current;
                DataOutputStream out=new DataOutputStream(current.getOutputStream());
                send(out,new JSONObject().put("type","hello").put("abi",3));
                current.setSoTimeout(5000);
                DataInputStream input=new DataInputStream(current.getInputStream());
                int helloLength=input.readInt();
                if(helloLength<=0 || helloLength>4096) throw new IllegalArgumentException();
                byte[] helloBytes=new byte[helloLength]; input.readFully(helloBytes);
                JSONObject hello=new JSONObject(new String(helloBytes,StandardCharsets.UTF_8));
                if(!"hello".equals(hello.optString("type")) || hello.optInt("abi")!=3) throw new IllegalArgumentException();
                current.setSoTimeout(0);
                connected=true;
                Thread writer=new Thread(() -> writeSnapshots(current,out),"droidloom-notifications-out");
                writer.setDaemon(true); writer.start();
                synchronized(lock) { dirty=true; lock.notifyAll(); }
                try {
                    while(!stopped) {
                        int length=input.readInt();
                        if(length<=0 || length>4096) throw new IllegalArgumentException();
                        byte[] bytes=new byte[length]; input.readFully(bytes);
                        JSONObject message=new JSONObject(new String(bytes,StandardCharsets.UTF_8));
                        main.post(() -> command(message));
                    }
                } finally {
                    connected=false; closeSocket(current);
                    synchronized(lock) { lock.notifyAll(); }
                    writer.join();
                }
            } catch(Exception e) {
                if(!stopped) Log.w(TAG,"Notification transport disconnected: "+e.getClass().getSimpleName());
            } finally { connected=false; socket=null; }
            if(!stopped) try { Thread.sleep(2000); } catch(InterruptedException ignored) { }
        }
    }
    private void writeSnapshots(HostChannel current, DataOutputStream out) {
        com.android.droidloom.runtime.CpuPlacement.background();
        LinkedHashMap<String,String> sent = new LinkedHashMap<>();
        boolean initial = true;
        try {
            while(!stopped && connected) {
                ArrayList<Entry> entries;
                HashSet<String> alerts;
                synchronized(lock) {
                    while(!stopped && connected && (!dirty || !listening)) lock.wait();
                    if(stopped || !connected) return;
                    entries=new ArrayList<>(active.values()); dirty=false;
                    alerts=new HashSet<>(alerting); alerting.clear();
                }
                LinkedHashMap<String,String> next = new LinkedHashMap<>();
                if(initial) send(out,new JSONObject().put("type","begin"));
                else {
                    // Retire entries before adding replacements so a full host set
                    // never transiently exceeds MAX_ACTIVE during a delta update.
                    HashSet<String> present = new HashSet<>();
                    for(Entry entry:entries) present.add(entry.sbn.getKey());
                    for(String key:sent.keySet()) if(!present.contains(key))
                        send(out,new JSONObject().put("type","remove").put("key",key).put("version",sent.get(key)));
                }
                for(Entry entry:entries) {
                    String key=entry.sbn.getKey();
                    if(!initial && entry.version.equals(sent.get(key))) {
                        next.put(key,entry.version); continue;
                    }
                    try {
                        send(out,new JSONObject().put("type","post").put("notification",encode(entry,alerts.contains(key))));
                        next.put(key,entry.version);
                    } catch(org.json.JSONException | android.content.pm.PackageManager.NameNotFoundException e) {
                        // Preserve the acknowledged version so a later event retries this entry.
                        // An app removed during encoding is retired by the following removal event.
                        if(sent.containsKey(key)) next.put(key,sent.get(key));
                    }
                }
                if(initial) send(out,new JSONObject().put("type","end"));
                sent=next; initial=false;
            }
        } catch(Exception e) { closeSocket(current); }
    }
    private JSONObject encode(Entry entry, boolean alert) throws Exception {
        StatusBarNotification sbn=entry.sbn; Notification n=sbn.getNotification(); Bundle extras=n.extras;
        String title=text(extras.getCharSequence(Notification.EXTRA_TITLE),2048);
        CharSequence big=extras.getCharSequence(Notification.EXTRA_BIG_TEXT);
        String body=text(big!=null?big:extras.getCharSequence(Notification.EXTRA_TEXT),8192);
        if(body.isEmpty()) {
            CharSequence[] lines=extras.getCharSequenceArray(Notification.EXTRA_TEXT_LINES);
            if(lines!=null) body=text(android.text.TextUtils.join("\n",lines),8192);
        }
        String component=component(sbn);
        JSONArray actions=new JSONArray();
        // Without a content intent, a tap still opens the app at its launcher activity.
        if(n.contentIntent!=null || !component.isEmpty()) actions.put(new JSONObject().put("key","default").put("label","Open"));
        if(n.actions!=null) for(int i=0;i<Math.min(n.actions.length,16);i++) {
            Notification.Action action=n.actions[i];
            // The standard D-Bus interface has no portable inline-reply field.
            if(action.actionIntent!=null && action.getRemoteInputs()==null)
                actions.put(new JSONObject().put("key","action-"+i).put("label",text(action.title,256)));
        }
        int max=extras.getInt(Notification.EXTRA_PROGRESS_MAX,0), value=extras.getInt(Notification.EXTRA_PROGRESS,0);
        int progress=max>0 && !extras.getBoolean(Notification.EXTRA_PROGRESS_INDETERMINATE)
                ? (int)Math.max(0,Math.min(100,(long)value*100/max)) : -1;
        String label=context.getPackageManager().getApplicationLabel(context.getPackageManager().getApplicationInfo(sbn.getPackageName(),0)).toString();
        return new JSONObject().put("key",sbn.getKey()).put("version",entry.version).put("package",sbn.getPackageName())
                .put("app",text(label,256)).put("title",title).put("body",body).put("actions",actions)
                // Android HIGH requests a heads-up alert; it does not mean a critical
                // desktop event that must remain visible until explicitly dismissed.
                .put("urgency",entry.importance<=NotificationManager.IMPORTANCE_LOW?0:1)
                .put("ongoing",sbn.isOngoing()).put("clearable",sbn.isClearable())
                .put("alert",alert)
                // Ungrouped notifications get a unique group key; the host groups those by app.
                .put("group",sbn.isGroup()?text(sbn.getGroupKey(),512):"")
                .put("summary",n.isGroupSummary())
                .put("when",n.when!=0?n.when:sbn.getPostTime())
                .put("progress",progress)
                .put("category",n.category==null?"":text(n.category,64))
                // A notification for every user (-1) shows as the current one's.
                .put("user",sbn.getNormalizedUserId()).put("component",component)
                // The host names the app icon through its desktop entry when it has one.
                .put("icon",component.isEmpty()?icon(sbn.getPackageName()):"")
                .put("large_icon",largeIcon(n))
                .put("messages",messages(extras));
    }
    /** The launcher activity Droidloom's desktop entry for this package names, or "". */
    private String component(StatusBarNotification sbn) {
        String cached=components.get(sbn.getPackageName()); if(cached!=null) return cached;
        Intent launch=launchIntent(sbn);
        String component=launch==null || launch.getComponent()==null ? "" : launch.getComponent().flattenToString();
        components.put(sbn.getPackageName(),component); return component;
    }
    private String icon(String pkg) {
        String cached=icons.get(pkg); if(cached!=null) return cached;
        String pixels="";
        try { pixels=pixels(context.getPackageManager().getApplicationIcon(pkg),48); } catch(Exception ignored) { }
        icons.put(pkg,pixels); return pixels;
    }
    private String largeIcon(Notification n) {
        if(n.getLargeIcon()==null) return "";
        try { return pixels(n.getLargeIcon().loadDrawable(context),96); } catch(Exception e) { return ""; }
    }
    private static JSONArray messages(Bundle extras) throws org.json.JSONException {
        JSONArray messages=new JSONArray();
        android.os.Parcelable[] raw=extras.getParcelableArray(Notification.EXTRA_MESSAGES);
        if(raw==null) return messages;
        java.util.List<Notification.MessagingStyle.Message> all=Notification.MessagingStyle.Message.getMessagesFromBundleArray(raw);
        for(int i=Math.max(0,all.size()-5);i<all.size();i++) {
            Notification.MessagingStyle.Message message=all.get(i);
            CharSequence sender=message.getSenderPerson()!=null?message.getSenderPerson().getName():message.getSender();
            messages.put(new JSONObject().put("sender",text(sender,256)).put("text",text(message.getText(),1024)));
        }
        return messages;
    }
    /** RGBA8888 as hex: half the size of a JSON number array. */
    private static String pixels(Drawable drawable,int size) {
        Bitmap bitmap=Bitmap.createBitmap(size,size,Bitmap.Config.ARGB_8888);
        drawable.setBounds(0,0,size,size); drawable.draw(new Canvas(bitmap));
        int[] pixels=new int[size*size]; bitmap.getPixels(pixels,0,size,0,0,size,size); bitmap.recycle();
        StringBuilder out=new StringBuilder(size*size*8);
        for(int pixel:pixels) { hex(out,(pixel>>16)&255); hex(out,(pixel>>8)&255); hex(out,pixel&255); hex(out,(pixel>>>24)&255); }
        return out.toString();
    }
    private static void hex(StringBuilder out,int value) { out.append(Character.forDigit(value>>4,16)).append(Character.forDigit(value&15,16)); }
    private static String text(CharSequence source,int max) {
        if(source==null) return "";
        String value=source.toString().replace("\u0000","");
        if(value.length()<=max) return value;
        int end=max; if(Character.isHighSurrogate(value.charAt(end-1))) end--;
        return value.substring(0,end);
    }
    private void command(JSONObject message) {
        try {
            Entry entry;
            synchronized(lock) { entry=active.get(message.getString("key")); }
            if(entry==null || !entry.version.equals(message.getString("version"))) return;
            Notification n=entry.sbn.getNotification();
            String type=message.getString("type");
            if(type.equals("dismiss")) { if(entry.sbn.isClearable()) cancelNotification(entry.sbn.getKey()); return; }
            if(!type.equals("action")) return;
            String action=message.getString("action"); PendingIntent intent=null;
            if(action.equals("default")) intent=n.contentIntent;
            else if(action.startsWith("action-") && n.actions!=null) {
                int index=Integer.parseInt(action.substring(7));
                if(index>=0 && index<n.actions.length && n.actions[index].getRemoteInputs()==null) intent=n.actions[index].actionIntent;
            }
            ActivityOptions options=ActivityOptions.makeBasic().setPendingIntentBackgroundActivityStartMode(ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED);
            if(intent==null) {
                Intent launch=action.equals("default")?launchIntent(entry.sbn):null;
                if(launch!=null) context.startActivityAsUser(launch,options.toBundle(),entry.sbn.getUser());
                return;
            }
            intent.send(context,0,null,null,null,null,options.toBundle());
            if(action.equals("default") && (n.flags & Notification.FLAG_AUTO_CANCEL)!=0 && entry.sbn.isClearable()) cancelNotification(entry.sbn.getKey());
        } catch(Exception e) { Log.w(TAG,"Notification action unavailable: "+e.getClass().getSimpleName()); }
    }
    private Intent launchIntent(StatusBarNotification sbn) {
        Intent launch=context.getPackageManager().getLaunchIntentForPackage(sbn.getPackageName());
        if(launch!=null) launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK|Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
        return launch;
    }
    private static void send(DataOutputStream out,JSONObject message) throws Exception {
        byte[] bytes=message.toString().getBytes(StandardCharsets.UTF_8);
        if(bytes.length>MAX_FRAME) throw new IllegalArgumentException("Notification frame too large");
        out.writeInt(bytes.length); out.write(bytes); out.flush();
    }
    private static void closeSocket(HostChannel socket) {
        try { socket.shutdownInput(); } catch(Exception ignored) { }
        try { socket.shutdownOutput(); } catch(Exception ignored) { }
        try { socket.close(); } catch(Exception ignored) { }
    }
    private void disconnect() { connected=false; HostChannel current=socket; if(current!=null) closeSocket(current); synchronized(lock) { lock.notifyAll(); } }
    @Override public void close() { stopped=true; disconnect(); try { unregisterAsSystemService(); } catch(Exception ignored) { } }
    public void dump(PrintWriter out) { synchronized(lock) { out.println("DROIDLOOM_NOTIFICATIONS_ABI=3; notificationsListening="+listening+" notificationsConnected="+connected+" activeNotifications="+active.size()); } }
}
