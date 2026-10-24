package com.android.droidloom.notificationprobe;
import android.app.*;
import android.content.*;
import org.json.*;
public class Probe extends BroadcastReceiver {
    @Override public void onReceive(Context context,Intent intent) {
        try {
            NotificationManager manager=context.getSystemService(NotificationManager.class);
            NotificationChannel channel=new NotificationChannel("test","Droidloom integration test",NotificationManager.IMPORTANCE_HIGH);
            channel.setSound(null,null);channel.enableVibration(false);manager.createNotificationChannel(channel);
            String command=intent.getStringExtra("command");
            int id=Integer.parseInt(intent.getStringExtra("id")==null?"91001":intent.getStringExtra("id"));
            if("post".equals(command)) {
                PendingIntent open=PendingIntent.getBroadcast(context,id+100000,new Intent(context,ClickReceiver.class).putExtra("kind","default").putExtra("id",id).setFlags(Intent.FLAG_RECEIVER_FOREGROUND),PendingIntent.FLAG_UPDATE_CURRENT|PendingIntent.FLAG_IMMUTABLE);
                PendingIntent action=PendingIntent.getBroadcast(context,id,new Intent(context,ClickReceiver.class).putExtra("id",id).setFlags(Intent.FLAG_RECEIVER_FOREGROUND),PendingIntent.FLAG_UPDATE_CURRENT|PendingIntent.FLAG_IMMUTABLE);
                String body=intent.getStringExtra("body"); if(body==null) body="Android → desktop notification ✓";
                String title=intent.getStringExtra("title"); if(title==null) title="Droidloom notification test";
                // Optional extras: group, summary, progress, alertonce, nointent, sender (MessagingStyle).
                Notification.Builder builder=new Notification.Builder(context,"test").setSmallIcon(android.R.drawable.ic_dialog_info)
                    .setContentTitle(title).setContentText(body).setStyle(new Notification.BigTextStyle().bigText(body))
                    .setAutoCancel(true).setOngoing("true".equals(intent.getStringExtra("ongoing")))
                    .setOnlyAlertOnce("true".equals(intent.getStringExtra("alertonce")))
                    .addAction(new Notification.Action.Builder(null,"Test action",action).build());
                if(!"true".equals(intent.getStringExtra("nointent"))) builder.setContentIntent(open);
                String group=intent.getStringExtra("group");
                if(group!=null) builder.setGroup(group).setGroupSummary("true".equals(intent.getStringExtra("summary")));
                String progress=intent.getStringExtra("progress");
                if(progress!=null) builder.setProgress(100,Integer.parseInt(progress),false);
                String sender=intent.getStringExtra("sender");
                if(sender!=null) builder.setStyle(new Notification.MessagingStyle(new Person.Builder().setName("Me").build())
                    .setConversationTitle(title).addMessage(body,System.currentTimeMillis(),new Person.Builder().setName(sender).build()));
                manager.notify(id,builder.build());
            } else if("cancel".equals(command)) manager.cancel(id);
            else if("clear".equals(command)) { manager.cancelAll(); context.getSharedPreferences("result",0).edit().clear().commit(); }
            else if(!"status".equals(command)) throw new IllegalArgumentException();
            JSONArray ids=new JSONArray(); for(android.service.notification.StatusBarNotification n:manager.getActiveNotifications()) ids.put(n.getId());
            setResultCode(0);setResultData(new JSONObject().put("ok",true).put("active",ids).put("lastAction",context.getSharedPreferences("result",0).getString("action","")).toString());
        } catch(Exception e) {setResultCode(1);setResultData("{\"ok\":false}");}
    }
}
