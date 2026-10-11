public class Lit {
    static int g(int a) {
        a += 100000;
        int b = a * 7;
        return b - 3;
    }

    static int h(int a, int b, int c) {
        return a * b + c;
    }

    public static void main(String[] q) {
        System.out.println(g(1) + " " + h(2, 3, 4));
    }
}