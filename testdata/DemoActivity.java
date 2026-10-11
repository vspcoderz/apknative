package com.example.demo;

public class DemoActivity {
    static int gcd(int a, int b) {
        while (b != 0) {
            int t = b;
            b = a % b;
            a = t;
        }
        return a;
    }

    public void onCreate() {
        System.out.println("DemoActivity.onCreate()");
        System.out.println("platform-independent work only");

        int total = 0;
        for (int i = 1; i <= 15; i++) {
            total += i;
        }
        System.out.println("sum 1..15 = " + total);

        String s = "apknative";
        System.out.println("len=" + s.length() + " upper=" + s.toUpperCase());
        System.out.println("part=" + s.substring(0, 4));
        System.out.println("gcd(48,18) = " + gcd(48, 18));

        int[] xs = new int[5];
        for (int i = 0; i < xs.length; i++) {
            xs[i] = i * i;
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < xs.length; i++) {
            sb.append(i).append("^2=").append(xs[i]).append(" ");
        }
        System.out.println(sb.toString());

        long acc = 1L;
        for (int i = 0; i < 40; i++) {
            acc = acc * 2;
        }
        System.out.println("2^40 = " + acc);
        System.out.println("activity finished");
    }

    public void onStart() {
        System.out.println("DemoActivity.onStart()");
    }

    public void onResume() {
        System.out.println("DemoActivity.onResume()");

        // These bodies all return null. If any of them ran, the equality
        // checks below would print false -- so they also prove the framework
        // overrides are consulted before DEX method resolution.
        Object a = getApplication();
        Object b = getApplication();
        System.out.println("application stable = " + (a == b));

        Object r1 = getResources();
        Object r2 = getResources();
        System.out.println("resources stable = " + (r1 == r2));

        Object s1 = getSystemService("layout_inflater");
        Object s2 = getSystemService("layout_inflater");
        System.out.println("service stable = " + (s1 == s2));
        System.out.println("distinct services differ = " + (s1 != getSystemService("window")));

        System.out.println("package = " + getPackageName());
    }

    // Declared only so this file compiles. apknative answers these from its
    // framework overrides, which run before any DEX method is resolved.
    public Object getApplication() { return null; }
    public Object getResources() { return null; }
    public Object getSystemService(String name) { return null; }
    public String getPackageName() { return null; }
}
