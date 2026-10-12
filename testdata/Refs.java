// Regression tests for reference semantics in the interpreter.
//
// Each check below corresponds to a bug that was found by running a real app,
// where none of them showed up as a failure in the ordinary test suite -- they
// produced *plausible* wrong answers instead.
//
//   if (x == null)      if-eqz coerced the register to a number, and a
//                       reference coerces to 0, so every non-null object read
//                       as null.
//   a == b               if-eq/if-ne compared the same coerced 0 == 0, so
//                       every object compared equal to every other.
//   iface.call()         invoke-interface dispatched on the method_id, which
//                       names an abstract method with no code body.
//
// The last group matters most: Compose builds the entire UI out of
// interface calls, and without this every composable body was silently
// invoked on nothing.

interface Greeter {
    String greet(String who);

    default String shout(String who) {
        return greet(who).toUpperCase();
    }
}

public class Refs {
    static class Counter {
        int n;

        Counter(int n) {
            this.n = n;
        }

        int next() {
            return ++n;
        }
    }

    public static void main(String[] args) {
        // ---- null tests -------------------------------------------------
        Object nothing = null;
        Object something = new Object();
        System.out.println("null == null: " + (nothing == null));
        System.out.println("obj == null: " + (something == null));
        System.out.println("null != obj: " + (nothing != something));
        System.out.println("obj != null: " + (something != null));

        // ---- identity ---------------------------------------------------
        Object a = new Counter(0);
        Object b = new Counter(0);
        System.out.println("distinct: " + (a != b));
        System.out.println("self: " + (a == a));
        System.out.println("equal-valuing: " + (a != b));

        // String literals are interned on the JVM; the interesting property is
        // that equality must not collapse unrelated strings.
        String s1 = new String("hello");
        String s2 = new String("hello");
        System.out.println("same content: " + s1.equals(s2));
        System.out.println("same content ==: " + (s1 == s2));

        // ---- if-eqz inside a loop, where the branch decides the count ----
        Object[] items = { null, new Object(), null, new Object() };
        int present = 0;
        for (int i = 0; i < items.length; i++) {
            if (items[i] != null) {
                present++;
            }
        }
        System.out.println("present: " + present);

        // ---- invoke-interface, anonymous implementation ------------------
        Greeter anon = new Greeter() {
            public String greet(String who) {
                return "hi " + who;
            }
        };
        System.out.println(anon.greet("there"));
        System.out.println(anon.shout("there"));

        // ---- invoke-interface through a local variable, not a field ------
        Greeter local = new Greeter() {
            public String greet(String who) {
                return "yo " + who;
            }
        };
        System.out.println(local.greet("bob"));

        // ---- a lambda is an interface implementation too -----------------
        Greeter lambda = who -> "lambda " + who;
        System.out.println(lambda.greet("call"));
        System.out.println(lambda.shout("call"));

        // ---- dispatch through an interface-typed field -------------------
        Holder h = new Holder();
        h.g = anon;
        System.out.println("via field: " + h.g.greet("field"));

        // ---- a null interface value is still null ------------------------
        h.g = null;
        System.out.println("field cleared: " + (h.g == null));

        // ---- state survives calls that return references -----------------
        Counter c = new Counter(10);
        System.out.println(c.next());
        System.out.println(c.next());
        System.out.println(c.next());
    }

    static class Holder {
        Greeter g;
    }
}