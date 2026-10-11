public class Hello {
    static int fib(int n) {
        if (n < 2) {
            return n;
        }
        return fib(n - 1) + fib(n - 2);
    }

    static int sumTo(int limit) {
        int sum = 0;
        for (int i = 1; i <= limit; i++) {
            sum += i;
        }
        return sum;
    }

    public static void main(String[] args) {
        System.out.println("Hello from Dalvik bytecode!");
        System.out.println("sum 1..10 = " + sumTo(10));
        System.out.println("fib(20)   = " + fib(20));

        String s = "apknative";
        System.out.println("len=" + s.length());
        System.out.println("upper=" + s.toUpperCase());
        System.out.println("concat=" + s.concat(" rocks"));

        int[] nums = new int[5];
        for (int i = 0; i < 5; i++) {
            nums[i] = i * i;
        }
        System.out.println("squares up to 4:");
        for (int i = 0; i < nums.length; i++) {
            System.out.println("  " + i + "^2 = " + nums[i]);
        }

        long big = 1L;
        for (int i = 0; i < 20; i++) {
            big = big * 2;
        }
        System.out.println("2^20 = " + big);
    }
}