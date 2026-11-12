#define _CRT_SECURE_NO_WARNINGS
/* gen <input> <output> <name>: writes a header defining <name>_text as the input's first line */
#include <stdio.h>
#include <string.h>
int main(int argc, char** argv)
{
    char line[256] = "";
    FILE* input;
    FILE* output;
    if (argc != 4) return 2;
    input = fopen(argv[1], "r");
    if (!input) return 3;
    if (!fgets(line, sizeof line, input)) line[0] = 0;
    fclose(input);
    line[strcspn(line, "\r\n")] = 0;
    output = fopen(argv[2], "w");
    if (!output) return 4;
    fprintf(output, "#define %s_text \"%s\"\n", argv[3], line);
    fclose(output);
    return 0;
}
