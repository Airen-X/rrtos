/* STM32H743 (Nucleo-H743ZI 等常见开发板) 内存布局。
 * 若你的具体型号/链接脚本不同，请相应修改 ORIGIN/LENGTH。 */
MEMORY
{
    /* 片内 Flash：2 MiB */
    FLASH : ORIGIN = 0x08000000, LENGTH = 2M

    /* DTCM RAM：128 KiB（栈从此处向下生长，cortex-m-rt 默认用 RAM 顶部做初始栈） */
    RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}
