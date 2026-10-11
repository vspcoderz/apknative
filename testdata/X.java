public class X {
    static int f(int a, int b) {
        int c = a + b;
        c = c + 100000;
        c = c * 3;
        c = a - b;
        c = c / 2;
        c = c % 5;
        return c << 2;
    }

    public static void main(String[] q) {
        System.out.println(f(3, 4));
    }
}