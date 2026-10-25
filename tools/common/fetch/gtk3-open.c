/* gtk3-open: GTK 3's GtkFileChooserNative, which a GTK 3 program opens for
 * a file, and which goes to the desktop portal when GTK_USE_PORTAL=1. Built
 * by fetch-chrome.sh against the volume's own GTK, for the boot that shows
 * Ferrix's file chooser comes up for GTK 3 too (docs/CHROME.md §10). No
 * headers: the four calls it makes are declared here. */
#include <stdio.h>

typedef void *pointer;
extern int gtk_init_check(int *argc, char ***argv);
extern pointer gtk_file_chooser_native_new(const char *title, pointer parent,
                                           int action, const char *accept,
                                           const char *cancel);
extern int gtk_native_dialog_run(pointer dialog);
extern char *gtk_file_chooser_get_filename(pointer chooser);

int main(int argc, char **argv) {
    if (!gtk_init_check(&argc, &argv)) {
        fprintf(stderr, "gtk3-open: no display\n");
        return 2;
    }
    /* GTK_FILE_CHOOSER_ACTION_OPEN is 0; GTK_RESPONSE_ACCEPT is -3. */
    pointer dialog = gtk_file_chooser_native_new("Open a file (GTK 3)", NULL, 0,
                                                 "_Open", "_Cancel");
    int answer = gtk_native_dialog_run(dialog);
    if (answer == -3) {
        char *file = gtk_file_chooser_get_filename(dialog);
        printf("gtk3-open: chose %s\n", file ? file : "(nothing)");
    } else {
        printf("gtk3-open: cancelled (%d)\n", answer);
    }
    fflush(stdout);
    return 0;
}
