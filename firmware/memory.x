/* Reserve main-thread stack headroom for HTTP/MQTT polling and interrupts.
 * Async task storage consumes the same RAM region. Linking successfully with
 * only the remaining sliver of RAM as stack is not sufficient validation.
 * The HTTP future-size regression test separately limits handler growth.
 */
ASSERT(_stack_start_cpu0 - _stack_end_cpu0 >= 80 * 1024,
       "Insufficient main stack: reserve at least 80 KiB; reduce async task/response sizes");
