package dev.ibkr.gateway;

import java.io.IOException;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import javax.swing.JLabel;
import javax.swing.JMenu;
import javax.swing.JMenuBar;
import javax.swing.JMenuItem;
import javax.swing.JPanel;
import javax.swing.JTextField;
import javax.swing.JPasswordField;
import javax.swing.JTree;
import javax.swing.SwingUtilities;
import javax.swing.tree.DefaultMutableTreeNode;

public final class BridgeSelfTest {
    private BridgeSelfTest() {}

    private static void check(boolean condition, String description) {
        if (!condition) {
            throw new AssertionError(description);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 0 && args[0].equals("ui")) {
            try {
                GatewayBridge.main(Arrays.copyOfRange(args, 1, args.length));
            } catch (Throwable error) {
                error.printStackTrace(System.err);
                System.exit(80);
            }
            return;
        }
        if (args.length > 0 && args[0].equals("restart")) {
            Path directory = Path.of(args[1]);
            Field runtime = GatewayBridge.class.getDeclaredField("runtime");
            runtime.setAccessible(true);
            runtime.set(null, directory);
            Field generation = GatewayBridge.class.getDeclaredField("generation");
            generation.setAccessible(true);
            generation.set(null, "0123456789abcdef0123456789abcdef");
            Files.createDirectories(directory.resolve("settings").resolve("session123"));
            Files.writeString(directory.resolve("settings/session123/autorestart"), "fixture-not-a-real-token");
            GatewayBridge.writeRestartRecord("session123");
            System.exit(0);
        }
        String[] fields = {"LOGIN", "unicode-user", "p\u00e4ss\t\n", ""};
        check(Arrays.equals(GatewayBridge.decode(GatewayBridge.encode(fields)), fields), "protocol roundtrip");
        try {
            GatewayBridge.decode(new byte[] {0, 1, 0, 0, 0, 5, 42});
            throw new AssertionError("truncation accepted");
        } catch (IOException expected) {
            check(expected.getMessage() != null, "explicit parse error");
        }
        try {
            GatewayBridge.encode(new String[] {"x".repeat(65536)});
            throw new AssertionError("unbounded frame accepted");
        } catch (IOException expected) {
            check(expected.getMessage() != null, "explicit bound error");
        }
        try {
            GatewayBridge.writeRestartRecord("first;second");
            throw new AssertionError("Invalid unowned native restart accepted");
        } catch (IOException expected) {
            check(expected.getMessage() != null, "invalid native restart must be rejected");
        }
        SwingUtilities.invokeAndWait(() -> {
            JPanel panel = new JPanel();
            JLabel label = new JLabel("Socket port:");
            JTextField field = new JTextField("4002");
            label.setLabelFor(field);
            panel.add(label);
            panel.add(field);
            check(GatewayBridge.field(panel, "Socket port") == field, "labeled field");
            panel.add(new JPasswordField("sensitive-fixture-value"));
            check(!GatewayBridge.dialogText(panel).contains("sensitive-fixture-value"), "diagnostics exclude password fields");
            check(!GatewayBridge.dialogText(panel).contains("4002"), "diagnostics exclude text field values");
            JPanel challenge = new JPanel();
            challenge.add(new JLabel("Challenge code: 123 456"));
            challenge.add(new JLabel("654-321"));
            check(!GatewayBridge.dialogText(challenge).contains("123 456"), "segmented challenge is redacted");
            check(!GatewayBridge.dialogText(challenge).contains("654-321"), "segmented number is redacted");
            JPanel relogin = new JPanel();
            relogin.add(new JLabel("Your connection was lost. Would you like to re-login?"));
            relogin.add(new javax.swing.JButton("Re-login"));
            relogin.add(new javax.swing.JButton("Cancel"));
            check(GatewayBridge.connectionDialog("Re-login is required", relogin)
                    == GatewayBridge.ConnectionDialog.RELOGIN_REQUIRED, "known relogin prompt");
            JPanel splitRelogin = new JPanel();
            splitRelogin.add(new JLabel("Your connection was lost."));
            splitRelogin.add(new JLabel("Would you like to re-login?"));
            splitRelogin.add(new javax.swing.JButton("Re-login"));
            splitRelogin.add(new javax.swing.JButton("Cancel"));
            check(GatewayBridge.connectionDialog("Re-login is required", splitRelogin)
                    == GatewayBridge.ConnectionDialog.RELOGIN_REQUIRED, "multi-label relogin prompt");
            relogin.add(new JLabel("Another session with the same user name already exists"));
            check(GatewayBridge.connectionDialog("Re-login is required", relogin)
                    == GatewayBridge.ConnectionDialog.NONE, "session conflict is not transient recovery");
            JPanel disconnected = new JPanel();
            disconnected.add(new JLabel("Connection to server failed: Server disconnected, please try again"));
            disconnected.add(new javax.swing.JButton("OK"));
            check(GatewayBridge.connectionDialog("IBKR Gateway", disconnected)
                    == GatewayBridge.ConnectionDialog.SERVER_DISCONNECTED, "known server disconnect");
            JLabel authentication = new JLabel("Security code: 123 456");
            disconnected.add(authentication);
            check(GatewayBridge.connectionDialog("IBKR Gateway", disconnected)
                    == GatewayBridge.ConnectionDialog.NONE, "redaction must not hide authentication context from classification");
            disconnected.remove(authentication);
            disconnected.add(new javax.swing.JButton("Reconnect This Session"));
            check(GatewayBridge.connectionDialog("IBKR Gateway", disconnected)
                    == GatewayBridge.ConnectionDialog.NONE, "additional takeover action is not accepted");
            JMenuBar bar = new JMenuBar();
            JMenu menu = new JMenu("Configure");
            JMenuItem settings = new JMenuItem("Settings");
            menu.add(settings);
            bar.add(menu);
            check(GatewayBridge.button(bar, "Settings") == settings, "nested menu discovery");
            DefaultMutableTreeNode root = new DefaultMutableTreeNode("root");
            DefaultMutableTreeNode api = new DefaultMutableTreeNode("API");
            api.add(new DefaultMutableTreeNode("Settings"));
            root.add(api);
            JTree tree = new JTree(root);
            JPanel treePanel = new JPanel();
            treePanel.add(tree);
            check(GatewayBridge.selectTree(treePanel, "API", "Settings"), "API tree selection");
            check(!GatewayBridge.selectTree(treePanel, "missing"), "unknown tree is not selected");
        });
        System.out.println("Bridge protocol and Swing fixtures passed");
    }
}
