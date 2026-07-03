///Register `CH0_UPDATE` writer
pub type W = crate::W<Ch0UpdateSpec>;
impl core::fmt::Debug for crate::generic::Reg<Ch0UpdateSpec> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "(not readable)")
    }
}
impl W {}
/**CH0 update trigger (write any value to apply PARAM changes)

You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_update::W`](W). See [API](https://docs.rs/svd2rust/#read--modify--write-api).*/
pub struct Ch0UpdateSpec;
impl crate::RegisterSpec for Ch0UpdateSpec {
    type Ux = u32;
}
///`write(|w| ..)` method takes [`ch0_update::W`](W) writer structure
impl crate::Writable for Ch0UpdateSpec {
    type Safety = crate::Unsafe;
}
///`reset()` method sets CH0_UPDATE to value 0
impl crate::Resettable for Ch0UpdateSpec {}
