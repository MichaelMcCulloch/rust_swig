package com.mjm.rust;

import android.app.Application;
import android.util.Log;

/**
 * Created by evgeniy on 16.03.17.
 */

public final class MyApplication extends Application {
    private static final String TAG = "exm MyApplication";
    private static MyApplication sSelf;
    private Session mSession;

    public MyApplication() {
        super();
        sSelf = this;
    }

    public static MyApplication get() {
        return sSelf;
    }

    @Override
    public void onCreate() {
        Log.i(TAG, "onCreate");
        super.onCreate();
        try {
            System.loadLibrary("mobcore");
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "Load libary ERROR: " + e);
            return;
        }
        mSession = new Session();
    }

    public Session getSession() {
        return mSession;
    }
}
